//! Area guards (`route_layer`s): every route of an area requires the right
//! principal even if a handler forgets its extractor.
//!
//! * not logged in → `401` → `error_pages` turns it into a redirect to the
//!   login page (`?next=` back here);
//! * logged in as the wrong kind (client on `/admin`, staff on `/client`) → `403`.

use axum::extract::{FromRequestParts, Request};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::extract::{CurrentClient, CurrentStaff, PlatformAdmin};

async fn guard<E>(req: Request, next: Next) -> Response
where
    E: FromRequestParts<(), Rejection = crate::AppError>,
{
    let (mut parts, body) = req.into_parts();
    match E::from_request_parts(&mut parts, &()).await {
        Ok(_) => next.run(Request::from_parts(parts, body)).await,
        Err(e) => e.into_response(),
    }
}

/// `/client/*`: a client user of this tenant.
pub async fn require_client(req: Request, next: Next) -> Response {
    guard::<CurrentClient>(req, next).await
}

/// `/admin/*`: a staff user of this tenant. When the tenant requires 2FA for
/// staff (`security.staff_2fa_required`) and the user has not set it up yet,
/// every admin page redirects to `/auth/2fa/setup` first.
pub async fn require_staff(req: Request, next: Next) -> Response {
    let setup_needed = req
        .extensions()
        .get::<std::sync::Arc<crate::session::SessionInfo>>()
        .is_some_and(|s| s.mfa_setup_required && s.is_authenticated());
    if setup_needed {
        let next_path = req.uri().path().to_owned();
        let target = format!(
            "/auth/2fa/setup?next={}",
            url::form_urlencoded::byte_serialize(next_path.as_bytes()).collect::<String>()
        );
        return axum::response::Redirect::to(&target).into_response();
    }
    // A staff member who is impersonating a client and opens an admin page
    // (back button, bookmark) is offered to end the impersonation instead of
    // a bare 403: the session belongs to the client until then.
    let impersonating = req
        .extensions()
        .get::<std::sync::Arc<crate::session::SessionInfo>>()
        .is_some_and(|s| s.principal.is_impersonated());
    if impersonating && req.method() == axum::http::Method::GET {
        return axum::response::Redirect::to("/admin/impersonation/end").into_response();
    }
    guard::<CurrentStaff>(req, next).await
}

/// `/platform/*` (except login): a platform operator.
pub async fn require_platform_admin(req: Request, next: Next) -> Response {
    guard::<PlatformAdmin>(req, next).await
}
