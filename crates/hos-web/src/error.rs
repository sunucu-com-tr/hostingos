//! [`AppError`]: the error type of every handler and extractor.
//!
//! `AppError::into_response` only sets the status, a few headers and an
//! [`ErrorInfo`] response extension. The [`crate::middleware::error_pages`]
//! layer then turns that into
//!
//! * a branded HTML page (403 / 404 / 429 / 500 …) for browsers,
//! * an `HX-Trigger` toast for htmx requests,
//! * `application/problem+json` (RFC 9457) under `/api`,
//! * a redirect to the right login page for `401` outside the API.
//!
//! Internal details (`Internal`, database errors, integration failures) are
//! logged and never shown to users.

use std::time::Duration;

use axum::response::{IntoResponse, Response};
use hos_core::CoreError;
use http::{HeaderValue, StatusCode};

/// Handler result.
pub type AppResult<T> = Result<T, AppError>;

/// Error returned by handlers / extractors.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// 404.
    #[error("not found")]
    NotFound,
    /// 401 — not logged in (browser: redirect to login).
    #[error("authentication required")]
    Unauthorized,
    /// 403 — logged in but not allowed. The message is logged, not shown.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// 403 — missing / invalid CSRF token or cross-origin form post.
    #[error("invalid or missing CSRF token")]
    Csrf,
    /// 429.
    #[error("too many requests")]
    RateLimited {
        /// `Retry-After`.
        retry_after: Option<Duration>,
    },
    /// 400 with a message that is safe to show.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// 409 with a message that is safe to show.
    #[error("conflict: {0}")]
    Conflict(String),
    /// 413.
    #[error("payload too large")]
    PayloadTooLarge,
    /// 403 — switched off in the public demo (`crate::demo`).
    #[error("disabled in the demo")]
    DemoDisabled,
    /// 503 (an upstream integration failed / timed out). Message is logged only.
    #[error("service unavailable: {0}")]
    Unavailable(String),
    /// 500. Message is logged only.
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    /// Shorthand for [`AppError::Internal`].
    pub fn internal(msg: impl std::fmt::Display) -> Self {
        Self::Internal(msg.to_string())
    }

    /// Shorthand for [`AppError::BadRequest`].
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }

    /// Shorthand for [`AppError::Forbidden`].
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }

    /// HTTP status.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) | Self::Csrf | Self::DemoDisabled => StatusCode::FORBIDDEN,
            Self::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Stable machine-readable code (problem+json `code`, i18n `errors.<code>.*`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::Csrf => "csrf",
            Self::RateLimited { .. } => "rate_limited",
            Self::BadRequest(_) => "bad_request",
            Self::Conflict(_) => "conflict",
            Self::PayloadTooLarge => "payload_too_large",
            Self::DemoDisabled => "demo_disabled",
            Self::Unavailable(_) => "unavailable",
            Self::Internal(_) => "internal",
        }
    }

    /// Message that may be shown to the user, if any.
    pub fn public_detail(&self) -> Option<&str> {
        match self {
            Self::BadRequest(m) | Self::Conflict(m) => Some(m),
            _ => None,
        }
    }
}

/// Response extension describing an error; rendered by
/// [`crate::middleware::error_pages`].
#[derive(Debug, Clone)]
pub struct ErrorInfo {
    /// Status.
    pub status: StatusCode,
    /// [`AppError::code`].
    pub code: &'static str,
    /// Safe user-facing detail.
    pub detail: Option<String>,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        match &self {
            Self::Internal(m) => tracing::error!(error = %m, "internal error"),
            Self::Unavailable(m) => tracing::warn!(error = %m, "upstream unavailable"),
            Self::Forbidden(m) => tracing::info!(reason = %m, "forbidden"),
            Self::Csrf => tracing::info!("csrf check failed"),
            _ => {}
        }
        let info = ErrorInfo {
            status,
            code: self.code(),
            detail: self.public_detail().map(str::to_owned),
        };
        let mut res = (
            status,
            status.canonical_reason().unwrap_or("Error").to_owned(),
        )
            .into_response();
        if let Self::RateLimited {
            retry_after: Some(d),
        } = &self
            && let Ok(v) = HeaderValue::from_str(&d.as_secs().max(1).to_string())
        {
            res.headers_mut().insert(http::header::RETRY_AFTER, v);
        }
        res.extensions_mut().insert(info);
        res
    }
}

impl From<CoreError> for AppError {
    fn from(err: CoreError) -> Self {
        match err.http_status() {
            400 | 422 => Self::BadRequest(err.to_string()),
            401 => Self::Unauthorized,
            403 => Self::Forbidden(err.to_string()),
            404 => Self::NotFound,
            409 => Self::Conflict(err.to_string()),
            502..=504 => Self::Unavailable(err.to_string()),
            _ => Self::Internal(err.to_string()),
        }
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        CoreError::from(err).into()
    }
}

impl From<askama::Error> for AppError {
    fn from(err: askama::Error) -> Self {
        Self::Internal(format!("template: {err}"))
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self::Internal(format!("{err:#}"))
    }
}

impl From<hos_auth::AuthError> for AppError {
    fn from(err: hos_auth::AuthError) -> Self {
        CoreError::from(err).into()
    }
}

impl From<hos_core::IntegrationError> for AppError {
    fn from(err: hos_core::IntegrationError) -> Self {
        CoreError::from(err).into()
    }
}

impl From<tokio::task::JoinError> for AppError {
    fn from(err: tokio::task::JoinError) -> Self {
        Self::Internal(format!("blocking task: {err}"))
    }
}

impl From<hos_core::crypto::CryptoError> for AppError {
    fn from(err: hos_core::crypto::CryptoError) -> Self {
        Self::Internal(format!("crypto: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_error_mapping() {
        assert!(matches!(
            AppError::from(CoreError::not_found("x", 1)),
            AppError::NotFound
        ));
        assert!(matches!(
            AppError::from(CoreError::Unauthorized),
            AppError::Unauthorized
        ));
        assert!(matches!(
            AppError::from(CoreError::Internal("db password=x".into())),
            AppError::Internal(_)
        ));
        let e = AppError::from(CoreError::validation("bad"));
        assert_eq!(e.status(), StatusCode::BAD_REQUEST);
        assert!(e.public_detail().is_some());
        assert!(AppError::internal("secret").public_detail().is_none());
    }

    #[test]
    fn response_carries_info() {
        let res = AppError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        }
        .into_response();
        assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(res.headers()["retry-after"], "30");
        assert_eq!(
            res.extensions().get::<ErrorInfo>().unwrap().code,
            "rate_limited"
        );
    }
}
