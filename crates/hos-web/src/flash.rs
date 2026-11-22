//! One-shot flash messages in a signed cookie (`hos_flash`).
//!
//! ```ignore
//! // set (POST handler) — then redirect (Post/Redirect/Get):
//! return Ok(flash::redirect("/client", SetFlash::success(ctx.t("auth.reset.done"))));
//! // or combine with other response parts:
//! Ok((SetFlash::error(msg), Redirect::to("/x")).into_response())
//! ```
//!
//! The layout renders incoming messages ([`Flash`], also in `PageCtx.flashes`)
//! as toasts; the cookie is cleared once an HTML page has been rendered. For
//! htmx requests that do not redirect, messages are delivered as an
//! `HX-Trigger: {"hos:toast": …}` event instead (handled by `static/js/app.js`).

use std::convert::Infallible;

use axum::extract::{FromRequestParts, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, IntoResponseParts, Redirect, Response, ResponseParts};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hos_auth::session::{CookieAttributes, build_cookie, clear_cookie, cookie_value};
use hos_core::crypto::{hmac_sha256, verify_hmac_sha256};
use http::request::Parts;
use http::{HeaderValue, header::SET_COOKIE};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// Cookie name.
pub const FLASH_COOKIE: &str = "hos_flash";

/// Message severity (CSS: `.toast--<kind>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlashKind {
    /// Green.
    Success,
    /// Neutral / blue.
    Info,
    /// Amber.
    Warning,
    /// Red.
    Error,
}

impl FlashKind {
    /// CSS / JSON name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// One message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlashMessage {
    /// Severity.
    pub kind: FlashKind,
    /// Already translated text.
    pub text: String,
}

/// Incoming messages of this request (extractor).
#[derive(Debug, Clone, Default)]
pub struct Flash(pub Vec<FlashMessage>);

impl<S: Send + Sync> FromRequestParts<S> for Flash {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Infallible> {
        Ok(parts.extensions.get::<Flash>().cloned().unwrap_or_default())
    }
}

/// Messages to show on the next page (response part).
#[derive(Debug, Clone, Default)]
pub struct SetFlash(pub Vec<FlashMessage>);

impl SetFlash {
    /// One message.
    pub fn new(kind: FlashKind, text: impl Into<String>) -> Self {
        Self(vec![FlashMessage {
            kind,
            text: text.into(),
        }])
    }

    /// Success message.
    pub fn success(text: impl Into<String>) -> Self {
        Self::new(FlashKind::Success, text)
    }

    /// Info message.
    pub fn info(text: impl Into<String>) -> Self {
        Self::new(FlashKind::Info, text)
    }

    /// Warning message.
    pub fn warning(text: impl Into<String>) -> Self {
        Self::new(FlashKind::Warning, text)
    }

    /// Error message.
    pub fn error(text: impl Into<String>) -> Self {
        Self::new(FlashKind::Error, text)
    }

    /// Adds another message.
    #[must_use]
    pub fn and(mut self, kind: FlashKind, text: impl Into<String>) -> Self {
        self.0.push(FlashMessage {
            kind,
            text: text.into(),
        });
        self
    }
}

#[derive(Debug, Clone)]
struct PendingFlash(Vec<FlashMessage>);

impl IntoResponseParts for SetFlash {
    type Error = Infallible;

    fn into_response_parts(self, mut res: ResponseParts) -> Result<ResponseParts, Infallible> {
        res.extensions_mut().insert(PendingFlash(self.0));
        Ok(res)
    }
}

/// `303 See Other` to `to` with flash messages.
pub fn redirect(to: &str, flash: SetFlash) -> Response {
    (flash, Redirect::to(to)).into_response()
}

fn encode(key: &[u8; 32], messages: &[FlashMessage]) -> Option<String> {
    let mut msgs: Vec<FlashMessage> = messages.iter().take(5).cloned().collect();
    for m in &mut msgs {
        m.text = m.text.chars().take(500).collect();
    }
    let json = serde_json::to_vec(&msgs).ok()?;
    let payload = URL_SAFE_NO_PAD.encode(json);
    let mac = URL_SAFE_NO_PAD.encode(hmac_sha256(key, format!("flash:{payload}").as_bytes()));
    Some(format!("{payload}.{mac}"))
}

fn decode(key: &[u8; 32], value: &str) -> Option<Vec<FlashMessage>> {
    let (payload, mac) = value.split_once('.')?;
    let mac = URL_SAFE_NO_PAD.decode(mac).ok()?;
    if !verify_hmac_sha256(key, format!("flash:{payload}").as_bytes(), &mac) {
        return None;
    }
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()
}

/// Middleware: decodes incoming / encodes outgoing flash messages.
pub async fn flash_layer(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let incoming = req
        .headers()
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| cookie_value(h, FLASH_COOKIE))
        .map(|v| decode(&state.cookie_key, v).unwrap_or_default());
    let had_cookie = incoming.is_some();
    let is_htmx = req.headers().contains_key("hx-request");
    req.extensions_mut()
        .insert(Flash(incoming.unwrap_or_default()));

    let mut res = next.run(req).await;
    let pending = res.extensions_mut().remove::<PendingFlash>();
    let is_redirect = res.status().is_redirection() || res.headers().contains_key("hx-redirect");
    if let Some(PendingFlash(msgs)) = pending {
        if is_htmx && !is_redirect {
            let payload = serde_json::json!({ "hos:toast": msgs });
            if let Ok(v) = HeaderValue::from_str(&payload.to_string()) {
                res.headers_mut().insert("hx-trigger", v);
            }
        } else if let Some(value) = encode(&state.cookie_key, &msgs) {
            let cookie = build_cookie(
                FLASH_COOKIE,
                &value,
                &CookieAttributes {
                    secure: state.web.secure,
                    max_age_secs: Some(300),
                    ..CookieAttributes::default()
                },
                chrono::Utc::now(),
            );
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                res.headers_mut().append(SET_COOKIE, v);
            }
        }
        return res;
    }
    let rendered_html = res.status().is_success()
        && !is_htmx
        && res
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("text/html"));
    if had_cookie
        && (rendered_html || res.status().is_client_error())
        && let Ok(v) = HeaderValue::from_str(&clear_cookie(FLASH_COOKIE, state.web.secure, "/"))
    {
        res.headers_mut().append(SET_COOKIE, v);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tamper() {
        let key = [7u8; 32];
        let msgs = vec![FlashMessage {
            kind: FlashKind::Success,
            text: "Kaydedildi ✓".into(),
        }];
        let v = encode(&key, &msgs).unwrap();
        assert_eq!(decode(&key, &v).unwrap(), msgs);
        assert!(decode(&[8u8; 32], &v).is_none());
        let tampered = format!("x{v}");
        assert!(decode(&key, &tampered).is_none());
    }
}
