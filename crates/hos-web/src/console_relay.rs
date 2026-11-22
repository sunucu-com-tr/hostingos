//! Websocket relay of the embedded VM console (noVNC in the browser ⇄
//! HostingOS ⇄ hypervisor console).
//!
//! The browser never talks to the hypervisor and never receives a ticket,
//! token or VNC password: the page gets a one-time console token
//! (`hos_billing::Billing::vm_console_session`), opens a same-origin
//! websocket with it, and this module
//!
//! 1. refuses the upgrade unless the `Origin` is the page's own origin
//!    (cross-site websocket hijacking),
//! 2. claims the token (single use, ~60 s, bound to the signed-in user, its
//!    role and tenant; per-user concurrent session cap) and re-checks the
//!    service's ownership / status in hos-billing,
//! 3. asks the VM's driver for the upstream (destination from the stored
//!    integration and the provider API — never from the request), connects
//!    under the outbound policy with the integration's TLS setting and
//!    authenticates to the VNC server itself (`hos_virt::console`),
//! 4. speaks RFB to the browser with security type "None" (the browser is
//!    already authenticated by its session) and then relays bytes both ways,
//!    with an idle timeout (no input from the browser), an absolute session
//!    limit, message size limits, rate limits per direction and a byte cap,
//! 5. re-checks every [`RECHECK_INTERVAL`] that the holder may still use the
//!    console and closes it (code 4005) when not: the web session was logged
//!    out, revoked or expired (or the user locked), a sub-user lost the
//!    `services` area or a staff member `services.manage`, or the service is
//!    no longer the client's active service (suspended, terminated).
//!
//! Every session is a `vm_console_sessions` row: who, which service, when,
//! connect / end time, traffic and the close reason (the console audit).
//! Clipboard support is whatever noVNC does client-side over RFB cut-text.
//!
//! CSP: no change. The page loads noVNC from `/static` (`script-src
//! 'self'`) and connects to `ws(s)://<same host>/…`, which `connect-src
//! 'self'` covers (CSP Level 3 matches `ws:`/`wss:` for `'self'`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::http::header::{HOST, ORIGIN};
use axum::response::Response;
use futures::{SinkExt as _, StreamExt as _};
use hos_auth::rbac::{Permission, PermissionSet};
use hos_billing::vm_ops::{CONSOLE_SESSION_CAP, ConsoleGrant, ConsoleRequester};
use hos_core::CoreError;
use hos_core::ids::{ServiceId, SessionId, TenantId};
use hos_db::repo::identity;
use hos_virt::console::{ConnectOptions, UpstreamReader, UpstreamWriter};

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Largest browser → VM websocket message (input events, clipboard).
pub const MAX_BROWSER_MESSAGE: usize = 256 * 1024;
/// Close the session after this long without browser input.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// VM → browser rate (bytes/s) and burst.
const OUT_RATE: f64 = 16.0 * 1024.0 * 1024.0;
const OUT_BURST: f64 = 4.0 * 1024.0 * 1024.0;
/// Browser → VM rate (bytes/s) and burst.
const IN_RATE: f64 = 256.0 * 1024.0;
const IN_BURST: f64 = 1024.0 * 1024.0;
/// Total traffic of one session.
const MAX_SESSION_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// How often a running session re-checks that its holder may go on.
pub const RECHECK_INTERVAL: Duration = Duration::from_secs(10);

/// A websocket connect of the console page.
pub struct RelayRequest {
    /// Tenant of the request.
    pub tenant_id: TenantId,
    /// Service in the URL (must match the token's).
    pub service_id: ServiceId,
    /// Signed-in user (client or staff).
    pub who: ConsoleRequester,
    /// The web session the console was opened from.
    pub session_id: SessionId,
    /// One-time token from the page.
    pub token: String,
}

/// Whether `Origin` is the origin of the request itself (same host; https,
/// or http when the deployment does not use secure cookies, i.e. local
/// development). Browsers always send `Origin` on websocket handshakes.
pub fn origin_allowed(headers: &HeaderMap, secure: bool) -> bool {
    let (Some(origin), Some(host)) = (
        headers.get(ORIGIN).and_then(|v| v.to_str().ok()),
        headers.get(HOST).and_then(|v| v.to_str().ok()),
    ) else {
        return false;
    };
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    let scheme_ok = url.scheme() == "https" || (!secure && url.scheme() == "http");
    let Some(origin_host) = url.host_str() else {
        return false;
    };
    let origin_authority = match url.port() {
        Some(p) => format!("{origin_host}:{p}"),
        None => origin_host.to_owned(),
    };
    let host = host.trim();
    let host_no_default = host
        .strip_suffix(":443")
        .filter(|_| url.scheme() == "https")
        .or_else(|| host.strip_suffix(":80").filter(|_| url.scheme() == "http"))
        .unwrap_or(host);
    scheme_ok && origin_authority.eq_ignore_ascii_case(host_no_default)
}

/// Validates the request, claims the token, connects the upstream and
/// upgrades. Refusals are plain HTTP errors (403 / 409 / 503) before the
/// upgrade; the token is spent either way once it was presented.
pub async fn upgrade(
    state: &AppState,
    ws: WebSocketUpgrade,
    headers: &HeaderMap,
    req: RelayRequest,
) -> AppResult<Response> {
    if !origin_allowed(headers, state.web.secure) {
        return Err(AppError::Forbidden(
            "cross-origin console connection".into(),
        ));
    }
    let billing = &state.billing;
    let grant = billing
        .vm_console_connect(req.tenant_id, &req.token, &req.who, CONSOLE_SESSION_CAP)
        .await
        .map_err(|e| match e {
            hos_core::CoreError::Unauthorized => {
                AppError::Forbidden("invalid or expired console token".into())
            }
            other => AppError::from(other),
        })?;
    let session_id = grant.session.id;
    let end = |reason: &'static str| async move {
        let _ = billing
            .vm_console_end(req.tenant_id, session_id, 0, 0, reason)
            .await;
    };
    if grant.session.service_id != req.service_id {
        end("wrong_service").await;
        return Err(AppError::Forbidden(
            "console token of another service".into(),
        ));
    }
    let (reader, writer, _) =
        match hos_virt::console::connect(&grant.upstream, ConnectOptions::default()).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, service = %req.service_id, "console upstream");
                end("upstream_error").await;
                return Err(AppError::Unavailable("the console is not reachable".into()));
            }
        };
    tracing::info!(
        session = %session_id,
        service = %req.service_id,
        user = %req.who.user_id,
        staff = req.who.client_id.is_none(),
        "console session connected"
    );
    let state = state.clone();
    let tenant_id = req.tenant_id;
    let holder = Holder {
        tenant_id,
        service_id: req.service_id,
        session_id: req.session_id,
        user_id: req.who.user_id,
        client_id: req.who.client_id,
    };
    Ok(ws
        .max_message_size(MAX_BROWSER_MESSAGE)
        .max_frame_size(MAX_BROWSER_MESSAGE)
        .protocols(["binary"])
        .on_failed_upgrade(move |_| {
            tracing::debug!("console websocket upgrade failed");
        })
        .on_upgrade(move |socket| async move {
            let stats = relay(socket, reader, writer, &grant, || {
                ended_for(&state, &holder)
            })
            .await;
            let _ = state
                .billing
                .vm_console_end(
                    tenant_id,
                    session_id,
                    stats.bytes_in,
                    stats.bytes_out,
                    stats.reason,
                )
                .await;
            tracing::info!(
                session = %session_id,
                bytes_in = stats.bytes_in,
                bytes_out = stats.bytes_out,
                reason = stats.reason,
                "console session ended"
            );
        }))
}

/// Who holds a running session (re-checked while it runs).
struct Holder {
    tenant_id: TenantId,
    service_id: ServiceId,
    session_id: SessionId,
    user_id: hos_core::ids::UserId,
    /// `None`: staff.
    client_id: Option<hos_core::ids::ClientId>,
}

/// Why a running session must end now (`None`: it may go on). A failed
/// check ends it too.
async fn ended_for(state: &AppState, h: &Holder) -> Option<&'static str> {
    match recheck(state, h).await {
        Ok(reason) => reason,
        Err(e) => {
            tracing::warn!(error = %e, service = %h.service_id, "console re-check failed");
            Some("recheck_failed")
        }
    }
}

async fn recheck(state: &AppState, h: &Holder) -> AppResult<Option<&'static str>> {
    if !identity::session_held_by(&state.pool, h.session_id, h.user_id).await? {
        return Ok(Some("signed_out"));
    }
    let allowed = match h.client_id {
        Some(client_id) => {
            crate::routes::client::load_member_access(state, h.tenant_id, client_id, h.user_id)
                .await?
                .is_some_and(|a| a.allows("services"))
        }
        None => PermissionSet::from_grants(
            identity::user_permissions(&state.pool, h.tenant_id, h.user_id).await?,
        )
        .allows(Permission::SERVICES_MANAGE),
    };
    if !allowed {
        return Ok(Some("access_revoked"));
    }
    match state
        .billing
        .authorize_service(h.tenant_id, h.service_id, h.client_id)
        .await
    {
        Ok(_) => Ok(None),
        Err(CoreError::NotFound { .. } | CoreError::Forbidden(_)) => {
            Ok(Some("service_unavailable"))
        }
        Err(e) => Err(e.into()),
    }
}

/// Traffic and close reason of a finished session.
#[derive(Debug, Clone, Copy)]
pub struct RelayStats {
    /// Browser → VM bytes.
    pub bytes_in: u64,
    /// VM → browser bytes.
    pub bytes_out: u64,
    /// Why it ended.
    pub reason: &'static str,
}

/// Token bucket (`tokens` may go negative: the caller sleeps it off).
struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn new(rate: f64, burst: f64) -> Self {
        Self {
            rate,
            burst,
            tokens: burst,
            last: Instant::now(),
        }
    }

    async fn take(&mut self, n: usize) {
        let now = Instant::now();
        self.tokens =
            (self.tokens + now.duration_since(self.last).as_secs_f64() * self.rate).min(self.burst);
        self.last = now;
        self.tokens -= n as f64;
        if self.tokens < 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(-self.tokens / self.rate)).await;
        }
    }
}

/// Reads exactly `n` bytes of the browser's RFB handshake (messages may be
/// split or merged arbitrarily).
async fn read_browser(
    rx: &mut futures::stream::SplitStream<WebSocket>,
    buf: &mut Vec<u8>,
    n: usize,
) -> Option<Vec<u8>> {
    while buf.len() < n {
        match rx.next().await? {
            Ok(Message::Binary(b)) => buf.extend_from_slice(&b),
            Ok(Message::Ping(_) | Message::Pong(_)) => {}
            _ => return None,
        }
    }
    Some(buf.drain(..n).collect())
}

/// RFB handshake with the browser as the server side: version 3.8,
/// security type "None", result OK. Returns bytes the browser already sent
/// beyond the handshake (its `ClientInit`).
async fn browser_handshake(
    tx: &mut futures::stream::SplitSink<WebSocket, Message>,
    rx: &mut futures::stream::SplitStream<WebSocket>,
) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    tx.send(Message::Binary(
        hos_virt::console::version_message(8).to_vec().into(),
    ))
    .await
    .ok()?;
    let version = read_browser(rx, &mut buf, 12).await?;
    let minor = hos_virt::console::parse_version(&version)?;
    if minor >= 7 {
        tx.send(Message::Binary(vec![1u8, 1].into())).await.ok()?;
        let chosen = read_browser(rx, &mut buf, 1).await?;
        if chosen[0] != 1 {
            return None;
        }
        tx.send(Message::Binary(vec![0u8, 0, 0, 0].into()))
            .await
            .ok()?;
    } else {
        // 3.3: the server decides; "None" has no result message.
        tx.send(Message::Binary(vec![0u8, 0, 0, 1].into()))
            .await
            .ok()?;
    }
    Some(buf)
}

/// Runs a session until either side closes, a limit hits or `ended`
/// (called every [`RECHECK_INTERVAL`]) gives a reason to stop.
pub async fn relay<F, Fut>(
    socket: WebSocket,
    mut reader: UpstreamReader,
    mut writer: UpstreamWriter,
    grant: &ConsoleGrant,
    mut ended: F,
) -> RelayStats
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<&'static str>>,
{
    let (mut tx, mut rx) = socket.split();
    let bytes_in = Arc::new(AtomicU64::new(0));
    let bytes_out = Arc::new(AtomicU64::new(0));
    let stats = |reason| RelayStats {
        bytes_in: bytes_in.load(Ordering::Relaxed),
        bytes_out: bytes_out.load(Ordering::Relaxed),
        reason,
    };
    let Some(leftover) = browser_handshake(&mut tx, &mut rx).await else {
        writer.close().await;
        return stats("browser_handshake");
    };
    if !leftover.is_empty() && writer.send(&leftover).await.is_err() {
        return stats("upstream_closed");
    }
    let started = Instant::now();
    let last_input = Arc::new(AtomicU64::new(0));
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel::<Option<(u16, &'static str)>>(None);

    // VM → browser.
    let out_counter = bytes_out.clone();
    let in_counter = bytes_in.clone();
    let mut down = tokio::spawn(async move {
        let mut bucket = Bucket::new(OUT_RATE, OUT_BURST);
        loop {
            tokio::select! {
                biased;
                changed = stop_rx.changed() => {
                    let (code, reason) = if changed.is_ok() {
                        (*stop_rx.borrow()).unwrap_or((1000, "closed"))
                    } else {
                        (1000, "closed")
                    };
                    let _ = tx
                        .send(Message::Close(Some(CloseFrame { code, reason: reason.into() })))
                        .await;
                    return reason;
                }
                data = reader.recv() => match data {
                    Ok(Some(bytes)) => {
                        let total = out_counter.fetch_add(bytes.len() as u64, Ordering::Relaxed)
                            + bytes.len() as u64
                            + in_counter.load(Ordering::Relaxed);
                        if total > MAX_SESSION_BYTES {
                            let _ = tx
                                .send(Message::Close(Some(CloseFrame {
                                    code: 4002,
                                    reason: "traffic limit".into(),
                                })))
                                .await;
                            return "bandwidth_cap";
                        }
                        bucket.take(bytes.len()).await;
                        if tx.send(Message::Binary(bytes.into())).await.is_err() {
                            return "browser_closed";
                        }
                    }
                    Ok(None) | Err(_) => {
                        let _ = tx
                            .send(Message::Close(Some(CloseFrame {
                                code: 4001,
                                reason: "console closed".into(),
                            })))
                            .await;
                        return "upstream_closed";
                    }
                },
            }
        }
    });

    // Browser → VM.
    let input = last_input.clone();
    let in_counter = bytes_in.clone();
    let mut up = tokio::spawn(async move {
        let mut bucket = Bucket::new(IN_RATE, IN_BURST);
        let reason = loop {
            match rx.next().await {
                Some(Ok(Message::Binary(b))) => {
                    in_counter.fetch_add(b.len() as u64, Ordering::Relaxed);
                    input.store(started.elapsed().as_secs(), Ordering::Relaxed);
                    bucket.take(b.len()).await;
                    if writer.send(&b).await.is_err() {
                        break "upstream_closed";
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Text(_))) => break "protocol_error",
                Some(Ok(Message::Close(_))) | None => break "browser_closed",
                Some(Err(_)) => break "browser_error",
            }
        };
        writer.close().await;
        reason
    });

    let deadline = tokio::time::Instant::now() + grant.max_duration;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut recheck = tokio::time::interval_at(
        tokio::time::Instant::now() + RECHECK_INTERVAL,
        RECHECK_INTERVAL,
    );
    let reason = loop {
        tokio::select! {
            r = &mut down => break r.unwrap_or("internal_error"),
            r = &mut up => {
                let reason = r.unwrap_or("internal_error");
                let _ = stop_tx.send(Some((1000, reason)));
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut down).await;
                break reason;
            }
            _ = tokio::time::sleep_until(deadline) => {
                let _ = stop_tx.send(Some((4003, "max_duration")));
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut down).await;
                break "max_duration";
            }
            _ = tick.tick() => {
                let idle = started.elapsed().as_secs()
                    .saturating_sub(last_input.load(Ordering::Relaxed));
                if idle >= IDLE_TIMEOUT.as_secs() {
                    let _ = stop_tx.send(Some((4004, "idle")));
                    let _ = tokio::time::timeout(Duration::from_secs(2), &mut down).await;
                    break "idle";
                }
            }
            _ = recheck.tick() => {
                if let Some(reason) = ended().await {
                    let _ = stop_tx.send(Some((4005, reason)));
                    let _ = tokio::time::timeout(Duration::from_secs(2), &mut down).await;
                    break reason;
                }
            }
        }
    };
    down.abort();
    up.abort();
    stats(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(origin: Option<&str>, host: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(HOST, HeaderValue::from_str(host).unwrap());
        if let Some(o) = origin {
            h.insert(ORIGIN, HeaderValue::from_str(o).unwrap());
        }
        h
    }

    #[test]
    fn origin_must_be_the_page_itself() {
        assert!(origin_allowed(
            &headers(Some("https://panel.example.com"), "panel.example.com"),
            true
        ));
        assert!(origin_allowed(
            &headers(Some("https://panel.example.com"), "panel.example.com:443"),
            true
        ));
        assert!(origin_allowed(
            &headers(Some("http://localhost:8113"), "localhost:8113"),
            false
        ));
        for (origin, host, secure) in [
            (None, "panel.example.com", true),
            (Some("https://evil.example"), "panel.example.com", true),
            (
                Some("https://panel.example.com.evil.example"),
                "panel.example.com",
                true,
            ),
            (Some("http://panel.example.com"), "panel.example.com", true),
            (
                Some("https://panel.example.com:8443"),
                "panel.example.com",
                true,
            ),
            (Some("null"), "panel.example.com", true),
        ] {
            assert!(
                !origin_allowed(&headers(origin, host), secure),
                "{origin:?}"
            );
        }
    }
}
