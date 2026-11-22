//! Referral links of the affiliate programme: `GET /r/{code}[?to=/path]`.
//!
//! A visit records a click (once per visitor and day; the visitor is an HMAC
//! of IP, user agent and day under a key derived from `HOS_SECRET_KEY`, so
//! no address is stored) and sets the `hos_ref` cookie (`CODE.unix-time`,
//! `HttpOnly`, `SameSite=Lax`) for the tenant's cookie window. With
//! first-click attribution an unexpired cookie is kept; with last-click it is
//! replaced. The cookie is read when a client account is created
//! ([`attribute_new_client`]); hos-billing applies the self-referral rules.
//! Unknown codes, a closed programme or a rate-limited visitor are simply
//! redirected without a cookie.

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use hos_billing::affiliates::{AffiliateSettings, plausible_code};
use hos_core::ids::{TenantId, UserId};
use hos_db::repo::{affiliates as arepo, clients};
use serde::Deserialize;

use crate::error::AppResult;
use crate::extract::{ClientIp, UserAgent};
use crate::state::AppState;
use crate::tenant::Tenant;

/// Referral cookie name.
pub const COOKIE_NAME: &str = "hos_ref";

/// The referral cookie of a request: `(code, clicked at)`.
#[derive(Debug, Clone, Default)]
pub struct ReferralCookie(pub Option<(String, Option<DateTime<Utc>>)>);

/// Parses a `CODE.unix` cookie value.
pub fn parse_cookie(raw: &str) -> Option<(String, Option<DateTime<Utc>>)> {
    let (code, ts) = raw.split_once('.').unwrap_or((raw, ""));
    if !plausible_code(code) {
        return None;
    }
    let at = ts
        .parse::<i64>()
        .ok()
        .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0));
    Some((code.to_owned(), at))
}

impl<S: Send + Sync> FromRequestParts<S> for ReferralCookie {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .headers
                .get_all(COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .find_map(|h| hos_auth::session::cookie_value(h, COOKIE_NAME))
                .and_then(parse_cookie),
        ))
    }
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct VisitQuery {
    #[serde(default)]
    to: String,
}

fn visitor_hash(
    state: &AppState,
    tenant: TenantId,
    ip: &ClientIp,
    ua: &UserAgent,
    day: chrono::NaiveDate,
) -> String {
    let key = hos_core::crypto::hmac_sha256(
        state.config.secret_key.expose(),
        b"hostingos/affiliate-visitor/v1",
    );
    let msg = format!(
        "{tenant}\n{}\n{}\n{day}",
        ip.0,
        ua.0.as_deref().unwrap_or("")
    );
    hos_core::crypto::hmac_sha256_hex(&key, msg.as_bytes())
}

/// `GET /r/{code}`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn visit(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    ua: UserAgent,
    existing: ReferralCookie,
    Path(code): Path<String>,
    Query(q): Query<VisitQuery>,
    headers: axum::http::HeaderMap,
) -> AppResult<Response> {
    let target = crate::locale::safe_local_target(q.to.trim())
        .filter(|t| !t.is_empty())
        .unwrap_or("/")
        .to_owned();
    let go = Redirect::to(&target).into_response();
    if !plausible_code(&code)
        || crate::state::check_rate(&state.limits.referral, &ip.key("referral")).is_err()
    {
        return Ok(go);
    }
    let settings = AffiliateSettings::load(&state.pool, tenant.id).await?;
    if !settings.enabled {
        return Ok(go);
    }
    let Some(affiliate) = arepo::active_affiliate_by_code(&state.pool, tenant.id, &code).await?
    else {
        return Ok(go);
    };
    let now = Utc::now();
    let referrer_host = headers
        .get(axum::http::header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| url::Url::parse(r).ok())
        .and_then(|u| {
            u.host_str()
                .map(|h| h.chars().take(255).collect::<String>())
        });
    arepo::record_click(
        &state.pool,
        tenant.id,
        affiliate.id,
        &visitor_hash(&state, tenant.id, &ip, &ua, now.date_naive()),
        now.date_naive(),
        Some(&target.chars().take(255).collect::<String>()),
        referrer_host.as_deref(),
    )
    .await?;
    let keep = !settings.last_click
        && existing.0.as_ref().is_some_and(|(_, at)| {
            at.is_some_and(|t| t <= now && now - t < chrono::Duration::days(settings.cookie_days))
        });
    if keep {
        return Ok(go);
    }
    let cookie = hos_auth::session::build_cookie(
        COOKIE_NAME,
        &format!("{}.{}", affiliate.code, now.timestamp()),
        &hos_auth::session::CookieAttributes {
            secure: state.web.secure,
            max_age_secs: Some(settings.cookie_days * 24 * 3600),
            ..hos_auth::session::CookieAttributes::default()
        },
        now,
    );
    Ok(([(SET_COOKIE, cookie)], go).into_response())
}

/// Attributes the client owned by a newly created user to the referral
/// cookie's affiliate. Never fails the sign-up: errors are logged.
pub async fn attribute_new_client(
    state: &AppState,
    tenant_id: TenantId,
    user_id: UserId,
    referral: &ReferralCookie,
) {
    let Some((code, clicked_at)) = &referral.0 else {
        return;
    };
    let result = async {
        for c in clients::clients_of_user(&state.pool, tenant_id, user_id).await? {
            let owner = clients::client_membership(&state.pool, tenant_id, c.id, user_id)
                .await?
                .is_some_and(|m| m.is_owner);
            if owner {
                state
                    .billing
                    .attribute_referral(tenant_id, c.id, code, *clicked_at)
                    .await?;
            }
        }
        Ok::<_, hos_core::CoreError>(())
    }
    .await;
    if let Err(e) = result {
        tracing::warn!(%tenant_id, error = %e, "referral attribution failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_values() {
        assert_eq!(
            parse_cookie("ABCDEF2345.1700000000").map(|c| c.0),
            Some("ABCDEF2345".into())
        );
        assert!(parse_cookie("ABCDEF2345.1700000000").unwrap().1.is_some());
        assert!(parse_cookie("ABCDEF2345").unwrap().1.is_none());
        assert!(parse_cookie("bad code.1").is_none());
        assert!(parse_cookie("x.1").is_none());
    }
}
