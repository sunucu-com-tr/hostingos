//! The HTTP server (`hostingos serve`) and its request limits
//! (docs/DEPLOYMENT.md, [`HttpTimeouts`]):
//!
//! * [`serve`] — the accept loop. Like `axum::serve` (graceful shutdown,
//!   `ConnectInfo`, websocket upgrades), but with a timer, so hyper's
//!   header-read timeout (`HOS_HTTP_HEADER_TIMEOUT_SECS`) applies: a client
//!   that sends its request headers slowly is cut off. `axum::serve` sets no
//!   timer and therefore no header timeout at all.
//! * [`limits`] — middleware near the top of the stack: reads the whole
//!   request body into memory within `HOS_HTTP_BODY_TIMEOUT_SECS` (`408`)
//!   and `HOS_MAX_BODY_BYTES` (`413`), then gives the rest of the stack
//!   `HOS_HTTP_REQUEST_TIMEOUT_SECS` to produce the response (`503`; the
//!   request's work is dropped). Everything inside it — the demo reset gate
//!   (`crate::demo::layer`), the CSRF check, handlers — sees a complete
//!   body, so a client that trickles its upload holds nothing but its own
//!   connection. Response bodies and upgraded connections (the VM console
//!   websocket) are not limited.

use std::future::Future;
use std::io;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::BytesMut;
use futures::StreamExt as _;
use http::header::{CACHE_CONTROL, CONNECTION, CONTENT_LENGTH, RETRY_AFTER};
use http::{HeaderValue, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower::ServiceExt as _;

use crate::config::{HttpTimeouts, WebConfig};

/// Serves `app` on `listener` until `shutdown` resolves, then stops
/// accepting, lets running requests finish and returns once every
/// connection is closed. Each request carries `ConnectInfo<SocketAddr>`
/// (the TCP peer, [`crate::extract::ClientIp`]).
pub async fn serve<F>(
    listener: TcpListener,
    app: Router,
    timeouts: HttpTimeouts,
    shutdown: F,
) -> io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(timeouts.header);
    // Dropped when `shutdown` resolves: connections then shut down gracefully.
    let (signal_tx, signal_rx) = watch::channel(());
    tokio::spawn(async move {
        shutdown.await;
        drop(signal_rx);
    });
    // Every connection task holds a receiver; all gone = all closed.
    let (close_tx, close_rx) = watch::channel(());
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                Err(e) => {
                    accept_error(e).await;
                    continue;
                }
            },
            () = signal_tx.closed() => break,
        };
        let _ = stream.set_nodelay(true);
        let service = app
            .clone()
            .map_request(move |mut req: Request<hyper::body::Incoming>| {
                req.extensions_mut().insert(ConnectInfo(peer));
                req.map(Body::new)
            });
        let service = TowerToHyperService::new(service);
        let conn = builder
            .serve_connection_with_upgrades(TokioIo::new(stream), service)
            .into_owned();
        let signal_tx = signal_tx.clone();
        let close_rx = close_rx.clone();
        tokio::spawn(async move {
            let mut conn = std::pin::pin!(conn);
            let mut signalled = std::pin::pin!(signal_tx.closed());
            let mut shutting_down = false;
            loop {
                tokio::select! {
                    result = conn.as_mut() => {
                        if let Err(e) = result {
                            tracing::trace!(error = %e, %peer, "connection ended with an error");
                        }
                        break;
                    }
                    () = &mut signalled, if !shutting_down => {
                        shutting_down = true;
                        conn.as_mut().graceful_shutdown();
                    }
                }
            }
            drop(close_rx);
        });
    }
    drop(close_rx);
    drop(listener);
    close_tx.closed().await;
    Ok(())
}

/// As `axum::serve`: a reset or aborted handshake is the client's business;
/// anything else (`EMFILE` …) is logged and the loop waits a second.
async fn accept_error(e: io::Error) {
    if matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    ) {
        return;
    }
    tracing::error!(error = %e, "accept error");
    tokio::time::sleep(Duration::from_secs(1)).await;
}

/// State of [`limits`].
#[derive(Debug, Clone, Copy)]
pub struct RequestLimits {
    /// `HOS_HTTP_*_TIMEOUT_SECS`.
    pub timeouts: HttpTimeouts,
    /// `HOS_MAX_BODY_BYTES`.
    pub max_body_bytes: usize,
}

impl From<&WebConfig> for RequestLimits {
    fn from(web: &WebConfig) -> Self {
        Self {
            timeouts: web.http_timeouts,
            max_body_bytes: web.max_body_bytes,
        }
    }
}

/// Why a request body could not be read.
#[derive(Debug, PartialEq, Eq)]
enum BodyError {
    /// Over `HOS_MAX_BODY_BYTES`.
    TooLarge,
    /// Not complete within `HOS_HTTP_BODY_TIMEOUT_SECS`.
    TimedOut,
    /// The connection broke or the framing was invalid.
    Broken,
}

/// The body in memory, at most `limit` bytes.
async fn read_body(body: Body, limit: usize) -> Result<bytes::Bytes, BodyError> {
    let mut stream = body.into_data_stream();
    let mut buf = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| BodyError::Broken)?;
        if buf.len() + chunk.len() > limit {
            return Err(BodyError::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf.freeze())
}

/// Middleware (see the module docs).
pub async fn limits(State(limits): State<RequestLimits>, req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let declared = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let read = async {
        if declared.is_some_and(|n| n > limits.max_body_bytes as u64) {
            return Err(BodyError::TooLarge);
        }
        let read = read_body(body, limits.max_body_bytes);
        match limits.timeouts.body {
            Some(d) => tokio::time::timeout(d, read)
                .await
                .unwrap_or(Err(BodyError::TimedOut)),
            None => read.await,
        }
    };
    let bytes = match read.await {
        Ok(bytes) => bytes,
        Err(e) => {
            if e == BodyError::TimedOut {
                tracing::info!(
                    method = %parts.method,
                    path = %parts.uri.path(),
                    "request body not received in time (HOS_HTTP_BODY_TIMEOUT_SECS)"
                );
            }
            return body_error(e);
        }
    };
    let req = Request::from_parts(parts, Body::from(bytes));
    let Some(limit) = limits.timeouts.request else {
        return next.run(req).await;
    };
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    match tokio::time::timeout(limit, next.run(req)).await {
        Ok(res) => res,
        Err(_) => {
            tracing::warn!(
                %method,
                %path,
                limit_secs = limit.as_secs(),
                "request took too long and was stopped (HOS_HTTP_REQUEST_TIMEOUT_SECS)"
            );
            let mut res = (StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable").into_response();
            res.headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("30"));
            res.headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res
        }
    }
}

fn body_error(e: BodyError) -> Response {
    let mut res = match e {
        BodyError::TooLarge => crate::error::AppError::PayloadTooLarge.into_response(),
        BodyError::TimedOut => (StatusCode::REQUEST_TIMEOUT, "Request Timeout").into_response(),
        BodyError::Broken => (StatusCode::BAD_REQUEST, "Bad Request").into_response(),
    };
    // The rest of the body is not read: the connection cannot be reused.
    res.headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    res
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible as Never;
    use std::net::SocketAddr;
    use std::time::Instant;

    use axum::routing::{get, post};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;

    fn timeouts(header: u64, body: u64, request: u64) -> HttpTimeouts {
        let d = |s: u64| (s > 0).then(|| Duration::from_millis(s));
        HttpTimeouts {
            header: d(header),
            body: d(body),
            request: d(request),
        }
    }

    fn app(t: HttpTimeouts) -> Router {
        Router::new()
            .route(
                "/echo",
                post(|body: bytes::Bytes| async move { format!("{} bytes", body.len()) }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "late"
                }),
            )
            .route(
                "/peer",
                get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.ip().to_string() }),
            )
            .layer(axum::middleware::from_fn_with_state(
                RequestLimits {
                    timeouts: t,
                    max_body_bytes: 1024,
                },
                limits,
            ))
    }

    /// A body that sends `first` and then stalls (until dropped).
    fn stalled_body(
        first: &'static str,
    ) -> (Body, tokio::sync::mpsc::Sender<Result<bytes::Bytes, Never>>) {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let stream = futures::stream::once(async move {
            Ok::<_, Never>(bytes::Bytes::from_static(first.as_bytes()))
        })
        .chain(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        }));
        (Body::from_stream(stream), tx)
    }

    #[tokio::test]
    async fn a_trickling_body_is_cut_off_with_408() {
        let (body, _keep_open) = stalled_body("a=1&b=");
        let req = Request::post("/echo")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .unwrap();
        let started = Instant::now();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            app(timeouts(0, 300, 0)).oneshot(req),
        )
        .await
        .expect("answered within the body timeout")
        .unwrap();
        assert_eq!(res.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(res.headers()[CONNECTION], "close");
        assert!(started.elapsed() >= Duration::from_millis(300));
    }

    #[tokio::test]
    async fn complete_bodies_pass_and_oversized_ones_are_refused() {
        let ok = Request::post("/echo")
            .body(Body::from("x".repeat(1024)))
            .unwrap();
        let res = app(timeouts(0, 300, 1000)).oneshot(ok).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 100).await.unwrap();
        assert_eq!(&body[..], b"1024 bytes");
        // Streamed past the limit, and announced past the limit.
        let big = Request::post("/echo")
            .body(Body::from("x".repeat(1025)))
            .unwrap();
        let res = app(timeouts(0, 300, 1000)).oneshot(big).await.unwrap();
        assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let (body, _keep_open) = stalled_body("x");
        let announced = Request::post("/echo")
            .header(CONTENT_LENGTH, "999999")
            .body(body)
            .unwrap();
        let res = app(timeouts(0, 300, 1000))
            .oneshot(announced)
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "refused before reading"
        );
        // No body timeout configured: a complete body still passes.
        let ok = Request::post("/echo").body(Body::from("abc")).unwrap();
        let res = app(timeouts(0, 0, 0)).oneshot(ok).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_slow_handler_is_stopped_with_503() {
        let req = Request::get("/slow").body(Body::empty()).unwrap();
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            app(timeouts(0, 0, 200)).oneshot(req),
        )
        .await
        .expect("answered within the request timeout")
        .unwrap();
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers()[RETRY_AFTER], "30");
    }

    /// Starts [`serve`] on an ephemeral loopback port.
    async fn start(
        t: HttpTimeouts,
    ) -> (
        SocketAddr,
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(serve(listener, app(t), t, async move {
            let _ = stop_rx.await;
        }));
        (addr, stop_tx, server)
    }

    /// Reads until the server closes the connection; `None` if it stays open.
    async fn read_to_close(s: &mut tokio::net::TcpStream, wait: Duration) -> Option<String> {
        let mut out = Vec::new();
        let read = async {
            let mut buf = [0u8; 1024];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                }
            }
        };
        tokio::time::timeout(wait, read).await.ok()?;
        Some(String::from_utf8_lossy(&out).into_owned())
    }

    #[tokio::test]
    async fn headers_sent_slowly_are_cut_off() {
        let (addr, stop, server) = start(timeouts(300, 0, 0)).await;
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /peer HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        // The rest of the headers never comes: the server hangs up.
        let started = Instant::now();
        assert!(
            read_to_close(&mut s, Duration::from_secs(5))
                .await
                .is_some(),
            "connection closed"
        );
        assert!(started.elapsed() >= Duration::from_millis(200));

        // A complete request is answered, with the TCP peer as ConnectInfo.
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /peer HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let res = read_to_close(&mut s, Duration::from_secs(5)).await.unwrap();
        assert!(res.starts_with("HTTP/1.1 200"), "{res}");
        assert!(res.ends_with("127.0.0.1"), "{res}");

        // Graceful shutdown: the server returns once connections are gone.
        stop.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server stopped")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn without_a_header_timeout_slow_headers_wait() {
        let (addr, stop, server) = start(timeouts(0, 0, 0)).await;
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /peer HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        assert!(
            read_to_close(&mut s, Duration::from_millis(600))
                .await
                .is_none(),
            "still open"
        );
        // Finishing the headers gets the answer.
        s.write_all(b"Connection: close\r\n\r\n").await.unwrap();
        let res = read_to_close(&mut s, Duration::from_secs(5)).await.unwrap();
        assert!(res.starts_with("HTTP/1.1 200"), "{res}");
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
    }
}
