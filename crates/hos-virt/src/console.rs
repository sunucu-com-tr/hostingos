//! Server side of the HostingOS console relay: opens the VNC upstream a
//! provider described in a [`ConsoleUpstream`] and authenticates to it, so the
//! browser only ever talks to HostingOS and never sees a hypervisor ticket,
//! token or VNC password.
//!
//! * Transports: RFB over a websocket (`wss://`, Proxmox `vncwebsocket`,
//!   Hetzner `wss_url`), RFB over TCP (Virtualizor VNC) and an in-process
//!   simulated VNC server for the `mock` provider ([`sim`]).
//! * The destination always comes from the provider (the stored integration
//!   and its API's answer). It is checked against the outbound policy
//!   ([`OutboundPolicy`]): IP literals statically, names after DNS
//!   resolution (every address must be allowed — no DNS rebinding).
//! * TLS: certificates are verified with the platform verifier (as reqwest
//!   does for the provider API) unless the integration has `skip_tls_verify`
//!   ([`ConsoleUpstream::accept_invalid_certs`]).
//! * RFB (RFC 6143): versions 3.3, 3.7 and 3.8; security types "None" (1) and
//!   "VNC authentication" (2, DES challenge–response with the VNC password).
//!   After [`connect`] returns, the next bytes the upstream expects are the
//!   client's `ClientInit`: the relay offers the browser security type "None"
//!   and from then on forwards bytes unchanged.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use des::cipher::generic_array::GenericArray;
use des::cipher::{BlockEncrypt, KeyInit};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use hos_core::IntegrationError;
use hos_core::integrations::virt::{ConsoleTransport, ConsoleUpstream};
use hos_core::net::OutboundPolicy;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};

pub mod sim;

/// Largest single websocket message accepted from the upstream.
pub const MAX_UPSTREAM_MESSAGE: usize = 16 * 1024 * 1024;

/// Options of [`connect`].
#[derive(Debug, Clone, Copy)]
pub struct ConnectOptions {
    /// Which destinations may be contacted.
    pub policy: OutboundPolicy,
    /// Bound for connect + TLS + websocket upgrade + RFB handshake.
    pub timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            policy: OutboundPolicy::global(),
            timeout: Duration::from_secs(15),
        }
    }
}

/// What the upstream announced during the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// Negotiated RFB minor version (3, 7 or 8).
    pub minor: u8,
    /// Security type used upstream (1 = None, 2 = VNC authentication).
    pub security: u8,
}

// ---------------------------------------------------------------------------
// transports
// ---------------------------------------------------------------------------

trait IoStream: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> IoStream for T {}

/// TCP, optionally wrapped in TLS.
enum MaybeTls {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for MaybeTls {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTls {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

type Ws = WebSocketStream<MaybeTls>;

enum Conn {
    Ws(Box<Ws>),
    Io(Box<dyn IoStream>),
}

/// Read half of an authenticated upstream.
pub struct UpstreamReader {
    inner: ReaderInner,
    pending: Vec<u8>,
}

enum ReaderInner {
    Ws(SplitStream<Ws>),
    Io(ReadHalf<Box<dyn IoStream>>),
}

/// Write half of an authenticated upstream.
pub struct UpstreamWriter {
    inner: WriterInner,
}

enum WriterInner {
    Ws(SplitSink<Ws, Message>),
    Io(WriteHalf<Box<dyn IoStream>>),
}

impl std::fmt::Debug for UpstreamReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamReader").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for UpstreamWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamWriter").finish_non_exhaustive()
    }
}

impl UpstreamReader {
    /// Next chunk of RFB bytes; `None` when the upstream closed.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>, IntegrationError> {
        if !self.pending.is_empty() {
            return Ok(Some(std::mem::take(&mut self.pending)));
        }
        match &mut self.inner {
            ReaderInner::Ws(stream) => loop {
                match stream.next().await {
                    None => return Ok(None),
                    Some(Err(e)) => return Err(ws_error(e)),
                    Some(Ok(Message::Binary(b))) => return Ok(Some(b.to_vec())),
                    // VNC over websockets is binary; text is not RFB.
                    Some(Ok(Message::Text(_))) => {
                        return Err(IntegrationError::Transport(
                            "console upstream sent a text frame".into(),
                        ));
                    }
                    Some(Ok(Message::Close(_))) => return Ok(None),
                    Some(Ok(_)) => {}
                }
            },
            ReaderInner::Io(r) => {
                let mut buf = vec![0u8; 64 * 1024];
                let n = r.read(&mut buf).await.map_err(io_error)?;
                if n == 0 {
                    return Ok(None);
                }
                buf.truncate(n);
                Ok(Some(buf))
            }
        }
    }
}

impl UpstreamWriter {
    /// Sends RFB bytes upstream.
    pub async fn send(&mut self, data: &[u8]) -> Result<(), IntegrationError> {
        match &mut self.inner {
            WriterInner::Ws(sink) => sink
                .send(Message::Binary(data.to_vec().into()))
                .await
                .map_err(ws_error),
            WriterInner::Io(w) => {
                w.write_all(data).await.map_err(io_error)?;
                w.flush().await.map_err(io_error)
            }
        }
    }

    /// Closes the upstream (best effort).
    pub async fn close(&mut self) {
        match &mut self.inner {
            WriterInner::Ws(sink) => {
                let _ = sink.send(Message::Close(None)).await;
                let _ = sink.close().await;
            }
            WriterInner::Io(w) => {
                let _ = w.shutdown().await;
            }
        }
    }
}

fn io_error(e: io::Error) -> IntegrationError {
    if e.kind() == io::ErrorKind::TimedOut {
        IntegrationError::Timeout
    } else {
        IntegrationError::Transport(format!("console upstream: {e}"))
    }
}

fn ws_error(e: tungstenite::Error) -> IntegrationError {
    match e {
        tungstenite::Error::Http(resp) => {
            let status = resp.status().as_u16();
            IntegrationError::from_http_status(
                status,
                format!("console websocket upgrade refused (HTTP {status})"),
            )
        }
        tungstenite::Error::Io(e) => io_error(e),
        tungstenite::Error::Capacity(_) => {
            IntegrationError::Transport("console upstream message too large".into())
        }
        // Never format the request (it carries the API token).
        other => IntegrationError::Transport(format!(
            "console websocket error: {}",
            ws_error_kind(&other)
        )),
    }
}

fn ws_error_kind(e: &tungstenite::Error) -> &'static str {
    match e {
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => "closed",
        tungstenite::Error::Tls(_) => "TLS failure",
        tungstenite::Error::Protocol(_) => "protocol violation",
        tungstenite::Error::Url(_) => "invalid URL",
        tungstenite::Error::HttpFormat(_) => "invalid HTTP",
        tungstenite::Error::Utf8(_) => "invalid UTF-8",
        tungstenite::Error::WriteBufferFull(_) => "write buffer full",
        _ => "transport error",
    }
}

impl Conn {
    async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, IntegrationError> {
        match self {
            Self::Ws(ws) => loop {
                match ws.next().await {
                    None => return Ok(None),
                    Some(Err(e)) => return Err(ws_error(e)),
                    Some(Ok(Message::Binary(b))) => return Ok(Some(b.to_vec())),
                    Some(Ok(Message::Close(_))) => return Ok(None),
                    Some(Ok(Message::Text(_))) => {
                        return Err(IntegrationError::Transport(
                            "console upstream sent a text frame".into(),
                        ));
                    }
                    Some(Ok(_)) => {}
                }
            },
            Self::Io(io) => {
                let mut buf = vec![0u8; 16 * 1024];
                let n = io.read(&mut buf).await.map_err(io_error)?;
                if n == 0 {
                    return Ok(None);
                }
                buf.truncate(n);
                Ok(Some(buf))
            }
        }
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), IntegrationError> {
        match self {
            Self::Ws(ws) => ws
                .send(Message::Binary(data.to_vec().into()))
                .await
                .map_err(ws_error),
            Self::Io(io) => {
                io.write_all(data).await.map_err(io_error)?;
                io.flush().await.map_err(io_error)
            }
        }
    }

    fn split(self, pending: Vec<u8>) -> (UpstreamReader, UpstreamWriter) {
        match self {
            Self::Ws(ws) => {
                let (sink, stream) = (*ws).split();
                (
                    UpstreamReader {
                        inner: ReaderInner::Ws(stream),
                        pending,
                    },
                    UpstreamWriter {
                        inner: WriterInner::Ws(sink),
                    },
                )
            }
            Self::Io(io) => {
                let (r, w) = tokio::io::split(io);
                (
                    UpstreamReader {
                        inner: ReaderInner::Io(r),
                        pending,
                    },
                    UpstreamWriter {
                        inner: WriterInner::Io(w),
                    },
                )
            }
        }
    }
}

/// A buffered view of a [`Conn`] for the handshake (`read_exact`).
struct Handshaker {
    conn: Conn,
    buf: Vec<u8>,
}

impl Handshaker {
    async fn read_exact(&mut self, n: usize) -> Result<Vec<u8>, IntegrationError> {
        while self.buf.len() < n {
            match self.conn.read_chunk().await? {
                Some(chunk) => self.buf.extend_from_slice(&chunk),
                None => {
                    return Err(IntegrationError::Transport(
                        "console upstream closed during the VNC handshake".into(),
                    ));
                }
            }
        }
        let rest = self.buf.split_off(n);
        Ok(std::mem::replace(&mut self.buf, rest))
    }

    async fn read_u8(&mut self) -> Result<u8, IntegrationError> {
        Ok(self.read_exact(1).await?[0])
    }

    async fn read_u32(&mut self) -> Result<u32, IntegrationError> {
        let b = self.read_exact(4).await?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    async fn read_reason(&mut self) -> Result<String, IntegrationError> {
        let len = self.read_u32().await?.min(1024) as usize;
        let b = self.read_exact(len).await?;
        Ok(String::from_utf8_lossy(&b).chars().take(200).collect())
    }
}

// ---------------------------------------------------------------------------
// connect
// ---------------------------------------------------------------------------

/// Opens and authenticates the upstream. On success the returned halves carry
/// the RFB stream right after the security handshake.
pub async fn connect(
    upstream: &ConsoleUpstream,
    opts: ConnectOptions,
) -> Result<(UpstreamReader, UpstreamWriter, Handshake), IntegrationError> {
    if let Some(exp) = upstream.expires_at
        && exp <= chrono::Utc::now()
    {
        return Err(IntegrationError::Conflict(
            "the console ticket expired before it was used".into(),
        ));
    }
    let password = upstream.vnc_password.as_ref().map(|p| p.expose_str());
    let work = async {
        let conn = open_transport(upstream, opts.policy).await?;
        let mut hs = Handshaker {
            conn,
            buf: Vec::new(),
        };
        let handshake = rfb_client_handshake(&mut hs, password).await?;
        let Handshaker { conn, buf } = hs;
        let (r, w) = conn.split(buf);
        Ok::<_, IntegrationError>((r, w, handshake))
    };
    tokio::time::timeout(opts.timeout, work)
        .await
        .map_err(|_| IntegrationError::Timeout)?
}

async fn open_transport(
    upstream: &ConsoleUpstream,
    policy: OutboundPolicy,
) -> Result<Conn, IntegrationError> {
    match upstream.transport {
        ConsoleTransport::Simulated => {
            let password = upstream
                .vnc_password
                .clone()
                .unwrap_or_else(|| hos_core::secret::SecretString::from(""));
            Ok(Conn::Io(Box::new(sim::spawn(password, &upstream.url))))
        }
        ConsoleTransport::Tcp => {
            let (host, port) = split_host_port(&upstream.url)?;
            let tcp = connect_tcp(&host, port, policy).await?;
            Ok(Conn::Io(Box::new(tcp)))
        }
        ConsoleTransport::WebSocket => {
            let url = url::Url::parse(&upstream.url)
                .map_err(|_| IntegrationError::Transport("invalid console websocket URL".into()))?;
            let tls = match url.scheme() {
                "wss" => true,
                "ws" => false,
                _ => {
                    return Err(IntegrationError::Config(
                        "console websocket URL must be ws:// or wss://".into(),
                    ));
                }
            };
            if !url.username().is_empty() || url.password().is_some() {
                return Err(IntegrationError::Config(
                    "credentials in the console URL are not allowed".into(),
                ));
            }
            let host = url
                .host_str()
                .ok_or_else(|| IntegrationError::Config("console URL has no host".into()))?
                .to_owned();
            let port = url
                .port_or_known_default()
                .unwrap_or(if tls { 443 } else { 80 });
            let tcp = connect_tcp(&host, port, policy).await?;
            let stream = if tls {
                let server_name = rustls::pki_types::ServerName::try_from(
                    host.trim_start_matches('[')
                        .trim_end_matches(']')
                        .to_owned(),
                )
                .map_err(|_| IntegrationError::Config("invalid console host name".into()))?;
                let config = tls_config(upstream.accept_invalid_certs)?;
                let tls = tokio_rustls::TlsConnector::from(config)
                    .connect(server_name, tcp)
                    .await
                    .map_err(|e| {
                        IntegrationError::Transport(format!("console TLS handshake failed: {e}"))
                    })?;
                MaybeTls::Tls(Box::new(tls))
            } else {
                MaybeTls::Plain(tcp)
            };
            let mut request = url
                .as_str()
                .into_client_request()
                .map_err(|_| IntegrationError::Config("invalid console websocket URL".into()))?;
            for (name, value) in &upstream.headers {
                let name = tungstenite::http::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| IntegrationError::Config("invalid console header".into()))?;
                let mut value = HeaderValue::from_str(value.expose_str())
                    .map_err(|_| IntegrationError::Config("invalid console header".into()))?;
                value.set_sensitive(true);
                request.headers_mut().insert(name, value);
            }
            if !upstream.subprotocols.is_empty() {
                let v = HeaderValue::from_str(&upstream.subprotocols.join(", "))
                    .map_err(|_| IntegrationError::Config("invalid subprotocol".into()))?;
                request.headers_mut().insert("Sec-WebSocket-Protocol", v);
            }
            let config = WebSocketConfig::default()
                .max_message_size(Some(MAX_UPSTREAM_MESSAGE))
                .max_frame_size(Some(MAX_UPSTREAM_MESSAGE));
            let (ws, _resp) =
                tokio_tungstenite::client_async_with_config(request, stream, Some(config))
                    .await
                    .map_err(ws_error)?;
            Ok(Conn::Ws(Box::new(ws)))
        }
    }
}

fn split_host_port(s: &str) -> Result<(String, u16), IntegrationError> {
    let bad = || IntegrationError::Config(format!("invalid VNC endpoint {s:?}"));
    let (host, port) = s.trim().rsplit_once(':').ok_or_else(bad)?;
    let port: u16 = port.parse().map_err(|_| bad())?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() || port == 0 {
        return Err(bad());
    }
    Ok((host.to_owned(), port))
}

async fn connect_tcp(
    host: &str,
    port: u16,
    policy: OutboundPolicy,
) -> Result<TcpStream, IntegrationError> {
    let addrs: Vec<SocketAddr> = policy
        .resolve_host(host, port)
        .await
        .map_err(IntegrationError::from)?;
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.map_or_else(
        || IntegrationError::Transport("no console address to connect to".into()),
        |e| IntegrationError::Transport(format!("console connection failed: {e}")),
    ))
}

fn tls_config(accept_invalid_certs: bool) -> Result<Arc<rustls::ClientConfig>, IntegrationError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| IntegrationError::Config(format!("TLS setup: {e}")))?;
    let config = if accept_invalid_certs {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth()
    } else {
        use rustls_platform_verifier::BuilderVerifierExt as _;
        builder
            .with_platform_verifier()
            .map_err(|e| IntegrationError::Config(format!("TLS setup: {e}")))?
            .with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// `skip_tls_verify`: any certificate, signatures still checked.
#[derive(Debug)]
struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

// ---------------------------------------------------------------------------
// RFB
// ---------------------------------------------------------------------------

/// Parses a `ProtocolVersion` message (`RFB 003.008\n`) → minor version to
/// use (3, 7 or 8).
pub fn parse_version(b: &[u8]) -> Option<u8> {
    if b.len() != 12 || &b[..4] != b"RFB " || b[7] != b'.' || b[11] != b'\n' {
        return None;
    }
    let major: u32 = std::str::from_utf8(&b[4..7]).ok()?.parse().ok()?;
    let minor: u32 = std::str::from_utf8(&b[8..11]).ok()?.parse().ok()?;
    if major != 3 {
        return None;
    }
    Some(match minor {
        8.. => 8,
        7 => 7,
        _ => 3,
    })
}

/// The `ProtocolVersion` message for a minor version.
pub fn version_message(minor: u8) -> [u8; 12] {
    let mut out = *b"RFB 003.008\n";
    out[10] = b'0' + minor.min(9);
    out
}

/// RFB "VNC authentication": DES-encrypts the 16-byte challenge with the
/// password (first 8 bytes, zero padded, each byte bit-reversed) as key.
pub fn vnc_auth_response(password: &[u8], challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (k, p) in key.iter_mut().zip(password.iter()) {
        *k = p.reverse_bits();
    }
    let mut out = *challenge;
    // A DES key is always 8 bytes, so this cannot fail.
    if let Ok(cipher) = des::Des::new_from_slice(&key) {
        for chunk in out.as_chunks_mut::<8>().0 {
            let block = GenericArray::from_mut_slice(chunk);
            cipher.encrypt_block(block);
        }
    }
    out
}

async fn rfb_client_handshake(
    hs: &mut Handshaker,
    password: Option<&str>,
) -> Result<Handshake, IntegrationError> {
    let version = hs.read_exact(12).await?;
    let minor = parse_version(&version).ok_or_else(|| {
        IntegrationError::Transport("console upstream is not a VNC (RFB 3.x) server".into())
    })?;
    hs.conn.write(&version_message(minor)).await?;
    let security = if minor == 3 {
        match hs.read_u32().await? {
            0 => {
                let reason = hs.read_reason().await?;
                return Err(IntegrationError::Remote {
                    status: None,
                    code: None,
                    message: format!("VNC server refused the connection: {reason}"),
                });
            }
            t @ (1 | 2) => t as u8,
            other => {
                return Err(IntegrationError::Transport(format!(
                    "unsupported VNC security type {other}"
                )));
            }
        }
    } else {
        let count = hs.read_u8().await?;
        if count == 0 {
            let reason = hs.read_reason().await?;
            return Err(IntegrationError::Remote {
                status: None,
                code: None,
                message: format!("VNC server refused the connection: {reason}"),
            });
        }
        let types = hs.read_exact(count as usize).await?;
        let chosen = if types.contains(&2) && password.is_some() {
            2
        } else if types.contains(&1) {
            1
        } else if types.contains(&2) {
            return Err(IntegrationError::Config(
                "the VNC server wants a password the provider did not supply".into(),
            ));
        } else {
            return Err(IntegrationError::Transport(format!(
                "no supported VNC security type offered ({types:?})"
            )));
        };
        hs.conn.write(&[chosen]).await?;
        chosen
    };
    if security == 2 {
        let challenge = hs.read_exact(16).await?;
        let mut c = [0u8; 16];
        c.copy_from_slice(&challenge);
        let response = vnc_auth_response(password.unwrap_or_default().as_bytes(), &c);
        hs.conn.write(&response).await?;
    }
    let has_result = minor == 8 || security == 2;
    if has_result {
        let result = hs.read_u32().await?;
        if result != 0 {
            let reason = if minor == 8 {
                hs.read_reason().await.unwrap_or_default()
            } else {
                String::new()
            };
            return Err(IntegrationError::Auth(if reason.is_empty() {
                "VNC authentication failed".into()
            } else {
                format!("VNC authentication failed: {reason}")
            }));
        }
    }
    Ok(Handshake { minor, security })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version(b"RFB 003.008\n"), Some(8));
        assert_eq!(parse_version(b"RFB 003.889\n"), Some(8));
        assert_eq!(parse_version(b"RFB 003.007\n"), Some(7));
        assert_eq!(parse_version(b"RFB 003.003\n"), Some(3));
        assert_eq!(parse_version(b"RFB 004.000\n"), None);
        assert_eq!(parse_version(b"HTTP/1.1 200"), None);
        assert_eq!(&version_message(7), b"RFB 003.007\n");
    }

    #[test]
    fn vnc_des_matches_reference() {
        // FIPS 46 worked example: key 133457799BBCDFF1, plaintext
        // 0123456789ABCDEF → 85E813540F0AB405. VNC bit-reverses every
        // password byte to form the key, so the password is the reversed key.
        let password = [0xC8, 0x2C, 0xEA, 0x9E, 0xD9, 0x3D, 0xFB, 0x8F];
        let mut challenge = [0u8; 16];
        challenge[..8].copy_from_slice(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]);
        challenge[8..].copy_from_slice(&[0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]);
        let r = vnc_auth_response(&password, &challenge);
        assert_eq!(hex::encode(&r[..8]), "85e813540f0ab405");
        assert_eq!(hex::encode(&r[8..]), "85e813540f0ab405");
        // Only the first 8 password bytes count.
        assert_eq!(
            vnc_auth_response(b"password-and-more", &[7u8; 16]),
            vnc_auth_response(b"password", &[7u8; 16])
        );
    }

    #[test]
    fn host_port() {
        assert_eq!(
            split_host_port("192.0.2.10:5910").unwrap(),
            ("192.0.2.10".into(), 5910)
        );
        assert_eq!(
            split_host_port("[2001:db8::1]:5901").unwrap(),
            ("2001:db8::1".into(), 5901)
        );
        assert!(split_host_port("nohost").is_err());
        assert!(split_host_port("h:0").is_err());
    }
}
