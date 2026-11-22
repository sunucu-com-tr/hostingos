//! "Sign in with Google / GitHub".
//!
//! ## Flow (`hos-web`)
//!
//! 1. `GET /auth/oauth/{provider}`: `let (url, pending) = provider.authorize_url(&redirect_uri)`;
//!    store `pending.seal(&secret_box)?` in the [`PENDING_COOKIE`] (HttpOnly,
//!    `Max-Age` = [`PENDING_TTL_SECS`], SameSite=Lax so it survives the
//!    top-level redirect back) and redirect to `url`.
//! 2. `GET /auth/oauth/{provider}/callback?code&state`: `PendingAuth::open(&sb,
//!    cookie, now)?`, clear the cookie, then [`complete`] (= state check via
//!    [`PendingAuth::verify_callback`] + [`OAuthProvider::exchange`]).
//! 3. Look up `user_identities` by `(provider, provider_user_id)` and `users` by
//!    email, then act on [`link_decision`].
//!
//! ## Security decisions
//!
//! * **state** (CSRF on the callback), **PKCE S256** (code interception) for
//!   both providers, **nonce** (id_token replay) for Google. All three live
//!   only in the AES-GCM-sealed, 10-minute pending cookie — no server state.
//! * **Google: the id_token is verified with Google's JWKS** (RS256 only, key
//!   by `kid`, cached 1 h, refetched on an unknown `kid` at most once a
//!   minute) and its claims are checked: `iss`, `aud` (+`azp`), `exp`/`iat`
//!   with 60 s leeway, and `nonce`. We chose this over the userinfo endpoint
//!   because userinfo carries no `nonce`/`aud`, so it cannot bind the identity
//!   to *this* login attempt and *this* client; and verifying the signature
//!   (instead of relying only on TLS to the token endpoint, which OIDC Core
//!   §3.1.3.7 would allow) also protects against a misconfigured/overridden
//!   token endpoint or proxy. `email_verified` comes from the signed token.
//! * **GitHub** has no OIDC for user login: identity = numeric `/user.id`
//!   (stable; logins can be renamed), email = the **primary** entry of
//!   `/user/emails` with its `verified` flag. The public `/user.email` is only
//!   a fallback and is then always treated as unverified.
//! * Accounts are never linked by an unverified email ([`link_decision`]);
//!   linking to a local account whose own email was never verified requires
//!   revoking its password/sessions (pre-account-takeover defence).
//! * Errors never contain the code, tokens or the client secret; HTTP clients
//!   have timeouts and do not follow redirects.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration as StdDuration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use hos_core::config::OAuthAppConfig;
use hos_core::crypto::{SecretBox, constant_time_eq, generate_token, sha256};
use hos_core::ids::UserId;
use hos_core::secret::SecretString;
use hos_core::{CoreError, IntegrationError};
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use url::Url;

hos_core::string_enum! {
    /// OAuth provider (`user_identities.provider`).
    pub enum ProviderKind {
        /// Google (OpenID Connect).
        Google => "google",
        /// GitHub (OAuth 2.0).
        GitHub => "github",
    }
}

/// Name of the cookie carrying the sealed [`PendingAuth`].
pub const PENDING_COOKIE: &str = "hos_oauth";
/// Lifetime of a pending login attempt.
pub const PENDING_TTL_SECS: i64 = 600;
// Pre-rename "hf" label, kept on purpose: AAD of sealed pending-login cookies.
const PENDING_AAD: &[u8] = b"hf-auth/oauth-pending/v1";
const HTTP_TIMEOUT: StdDuration = StdDuration::from_secs(10);
const CONNECT_TIMEOUT: StdDuration = StdDuration::from_secs(5);
const USER_AGENT: &str = "HostingOS-Auth";
/// Clock skew tolerated when checking `exp`/`iat`.
const LEEWAY_SECS: i64 = 60;
const JWKS_TTL: StdDuration = StdDuration::from_secs(3600);
const JWKS_MIN_REFRESH: StdDuration = StdDuration::from_secs(60);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// OAuth failure. Never contains codes, tokens or secrets.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OAuthError {
    /// `state` missing or different from the pending cookie (possible CSRF).
    #[error("OAuth state mismatch")]
    StateMismatch,
    /// The pending login attempt is older than [`PENDING_TTL_SECS`].
    #[error("OAuth login attempt expired")]
    Expired,
    /// The pending cookie is missing, tampered with or undecryptable.
    #[error("OAuth pending data is invalid")]
    InvalidPending,
    /// The pending attempt was started for another provider.
    #[error("OAuth callback for {got} but the attempt was started for {expected}")]
    ProviderMismatch {
        /// Provider handling the callback.
        expected: ProviderKind,
        /// Provider recorded in the pending attempt.
        got: ProviderKind,
    },
    /// The provider redirected back with `error=…` (user cancelled, consent denied).
    #[error("authorization failed at provider: {error}")]
    AuthorizationDenied {
        /// OAuth error code (`access_denied`).
        error: String,
        /// Provider description, if any.
        description: Option<String>,
    },
    /// No `code` in the callback.
    #[error("authorization code missing from callback")]
    MissingCode,
    /// The token endpoint rejected the code (`invalid_grant`, `invalid_client`...).
    #[error("token exchange failed: {error}")]
    TokenExchange {
        /// HTTP status.
        status: Option<u16>,
        /// OAuth error code.
        error: String,
        /// Provider description, if any.
        description: Option<String>,
    },
    /// The Google id_token failed verification.
    #[error("invalid id_token: {0}")]
    InvalidIdToken(String),
    /// Unexpected/unparsable provider response.
    #[error("unexpected provider response: {0}")]
    InvalidResponse(String),
    /// A provider API call returned a non-success status.
    #[error("provider API error (HTTP {status})")]
    Api {
        /// HTTP status.
        status: u16,
    },
    /// Network/TLS failure.
    #[error("transport error: {0}")]
    Transport(String),
    /// Request timed out.
    #[error("request to provider timed out")]
    Timeout,
    /// Client construction / configuration problem.
    #[error("OAuth misconfigured: {0}")]
    Config(String),
}

impl From<OAuthError> for CoreError {
    fn from(err: OAuthError) -> Self {
        match err {
            OAuthError::StateMismatch
            | OAuthError::Expired
            | OAuthError::InvalidPending
            | OAuthError::ProviderMismatch { .. }
            | OAuthError::AuthorizationDenied { .. }
            | OAuthError::MissingCode => CoreError::Validation(err.to_string()),
            OAuthError::TokenExchange { ref error, .. }
                if error == "invalid_client" || error == "unauthorized_client" =>
            {
                CoreError::Integration(IntegrationError::Auth(err.to_string()))
            }
            OAuthError::TokenExchange { .. } => CoreError::Validation(err.to_string()),
            OAuthError::InvalidIdToken(_) => {
                CoreError::Integration(IntegrationError::Auth(err.to_string()))
            }
            OAuthError::InvalidResponse(_) => {
                CoreError::Integration(IntegrationError::Transport(err.to_string()))
            }
            OAuthError::Api { status } => {
                CoreError::Integration(IntegrationError::from_http_status(status, err.to_string()))
            }
            OAuthError::Transport(m) => CoreError::Integration(IntegrationError::Transport(m)),
            OAuthError::Timeout => CoreError::Integration(IntegrationError::Timeout),
            OAuthError::Config(m) => CoreError::Internal(m),
        }
    }
}

// ---------------------------------------------------------------------------
// Pending auth (sealed cookie)
// ---------------------------------------------------------------------------

/// Per-attempt secrets, carried in an encrypted short-lived cookie between
/// the redirect to the provider and the callback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAuth {
    /// Provider the attempt was started for.
    pub provider: ProviderKind,
    /// Random `state` (256 bits).
    pub state: String,
    /// PKCE code verifier (256 bits, base64url).
    pub pkce_verifier: SecretString,
    /// OIDC nonce (256 bits).
    pub nonce: String,
    /// Unix seconds when the attempt started.
    pub created_at: i64,
    /// Local path to continue to after login (validated by [`safe_return_path`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_to: Option<String>,
    /// Explicit "link this provider to my account" intent of a signed-in
    /// user. The callback must check that the same user is still signed in;
    /// it then links instead of logging in or creating an account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_user_id: Option<UserId>,
}

impl PendingAuth {
    /// New attempt with fresh random `state`, PKCE verifier and nonce.
    pub fn new(provider: ProviderKind, now: DateTime<Utc>) -> Self {
        Self {
            provider,
            state: generate_token(),
            pkce_verifier: SecretString::from(generate_token()),
            nonce: generate_token(),
            created_at: now.timestamp(),
            return_to: None,
            link_user_id: None,
        }
    }

    /// Marks the attempt as a link intent of the signed-in `user_id`.
    #[must_use]
    pub fn with_link_user(mut self, user_id: UserId) -> Self {
        self.link_user_id = Some(user_id);
        self
    }

    /// Sets `return_to` if `path` is a safe local path (ignored otherwise).
    #[must_use]
    pub fn with_return_to(mut self, path: &str) -> Self {
        self.return_to = safe_return_path(path).map(str::to_owned);
        self
    }

    /// PKCE S256 challenge: `base64url(sha256(verifier))`.
    pub fn pkce_challenge(&self) -> String {
        URL_SAFE_NO_PAD.encode(sha256(self.pkce_verifier.expose_str().as_bytes()))
    }

    /// Whether the attempt is outside its lifetime (or from the future).
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        let now = now.timestamp();
        now > self.created_at + PENDING_TTL_SECS || self.created_at > now + LEEWAY_SECS
    }

    /// Encrypts into a cookie-safe string (base64url of a [`SecretBox`] ciphertext).
    pub fn seal(&self, secret_box: &SecretBox) -> Result<String, OAuthError> {
        let sealed = secret_box
            .encrypt_json(self, PENDING_AAD)
            .map_err(|e| OAuthError::Config(e.to_string()))?;
        Ok(URL_SAFE_NO_PAD.encode(sealed))
    }

    /// Decrypts a cookie value produced by [`seal`](Self::seal) and checks expiry.
    pub fn open(
        secret_box: &SecretBox,
        sealed: &str,
        now: DateTime<Utc>,
    ) -> Result<Self, OAuthError> {
        if sealed.len() > 4096 {
            return Err(OAuthError::InvalidPending);
        }
        let raw = URL_SAFE_NO_PAD
            .decode(sealed.trim())
            .map_err(|_| OAuthError::InvalidPending)?;
        let pending: Self = secret_box
            .decrypt_json(&raw, PENDING_AAD)
            .map_err(|_| OAuthError::InvalidPending)?;
        if pending.is_expired(now) {
            return Err(OAuthError::Expired);
        }
        Ok(pending)
    }

    /// Validates the callback query against this attempt and returns the code.
    ///
    /// Order: expiry → `state` (constant time) → provider `error` → `code`.
    /// The state is checked before the error so a forged callback cannot even
    /// trigger an error page for the victim's attempt.
    pub fn verify_callback(
        &self,
        params: &CallbackParams,
        now: DateTime<Utc>,
    ) -> Result<String, OAuthError> {
        if self.is_expired(now) {
            return Err(OAuthError::Expired);
        }
        let state = params.state.as_deref().unwrap_or("");
        if state.is_empty() || !constant_time_eq(state.as_bytes(), self.state.as_bytes()) {
            return Err(OAuthError::StateMismatch);
        }
        if let Some(error) = params.error.as_deref().filter(|e| !e.is_empty()) {
            return Err(OAuthError::AuthorizationDenied {
                error: truncate(error, 64),
                description: params
                    .error_description
                    .as_deref()
                    .map(|d| truncate(d, 256)),
            });
        }
        params
            .code
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty() && c.len() <= 2048)
            .map(str::to_owned)
            .ok_or(OAuthError::MissingCode)
    }
}

/// Query parameters of the provider callback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CallbackParams {
    /// Authorization code.
    pub code: Option<String>,
    /// Echoed `state`.
    pub state: Option<String>,
    /// OAuth error code (`access_denied`).
    pub error: Option<String>,
    /// Error description.
    pub error_description: Option<String>,
}

/// Accepts only same-site relative paths (`/client/services?x=1`); rejects
/// absolute URLs, protocol-relative `//host`, backslashes and control chars
/// (open-redirect defence for `return_to`).
pub fn safe_return_path(path: &str) -> Option<&str> {
    let ok = path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.len() <= 512;
    ok.then_some(path)
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// Profile, provider trait, linking
// ---------------------------------------------------------------------------

/// The identity returned by a provider after a successful exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthProfile {
    /// Provider.
    pub provider: ProviderKind,
    /// Stable provider user id (Google `sub`, GitHub numeric id).
    pub provider_user_id: String,
    /// Email, trimmed and lower-cased.
    pub email: Option<String>,
    /// Whether the provider asserts the email is verified.
    pub email_verified: bool,
    /// Display name.
    pub name: Option<String>,
    /// Avatar URL.
    pub avatar_url: Option<String>,
}

/// An OAuth login provider. See the [module docs](self) for the flow.
#[async_trait]
pub trait OAuthProvider: Send + Sync {
    /// Which provider this is.
    fn kind(&self) -> ProviderKind;

    /// Authorization URL to redirect the browser to, plus the per-attempt
    /// secrets to seal into the pending cookie.
    fn authorize_url(&self, redirect_uri: &Url) -> (Url, PendingAuth);

    /// Exchanges the authorization `code` for the user's profile. Call only
    /// after [`PendingAuth::verify_callback`] (or use [`complete`]).
    /// `redirect_uri` must be identical to the one used in `authorize_url`.
    async fn exchange(
        &self,
        code: &str,
        pending: &PendingAuth,
        redirect_uri: &Url,
    ) -> Result<OAuthProfile, OAuthError>;
}

/// Callback handler core: [`PendingAuth::verify_callback`] then
/// [`OAuthProvider::exchange`].
pub async fn complete(
    provider: &dyn OAuthProvider,
    params: &CallbackParams,
    pending: &PendingAuth,
    redirect_uri: &Url,
    now: DateTime<Utc>,
) -> Result<OAuthProfile, OAuthError> {
    let code = pending.verify_callback(params, now)?;
    provider.exchange(&code, pending, redirect_uri).await
}

/// Builds the provider for `kind` with the given OAuth app (platform default
/// from env, or a tenant's own app) and the real endpoints.
pub fn build_provider(
    kind: ProviderKind,
    app: OAuthAppConfig,
) -> Result<Arc<dyn OAuthProvider>, OAuthError> {
    Ok(match kind {
        ProviderKind::Google => Arc::new(GoogleProvider::new(app)?),
        ProviderKind::GitHub => Arc::new(GitHubProvider::new(app)?),
    })
}

/// A local account found by email.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExistingAccount {
    /// `users.id`.
    pub user_id: UserId,
    /// Whether the local account's own email address was ever verified.
    pub email_verified: bool,
}

/// What to do with an OAuth profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkDecision {
    /// The identity is already linked: log this user in.
    Login {
        /// User to log in.
        user_id: UserId,
    },
    /// Link the identity to the account with the same (verified) email, then log in.
    LinkToExisting {
        /// User to link and log in.
        user_id: UserId,
        /// The local account never verified its email: someone may have
        /// pre-registered it. Delete its password hash, sessions, TOTP and API
        /// keys before logging in, and mark the email verified.
        revoke_local_credentials: bool,
    },
    /// No account yet: create one (email already verified by the provider).
    CreateNew,
    /// No linked identity and the provider did not assert a verified email:
    /// refuse (ask the user to verify the email at the provider, or log in
    /// with a password and link from the account settings).
    RejectUnverifiedEmail,
}

/// Pure linking policy. `existing_by_identity` = user owning the
/// `user_identities` row for `(provider, provider_user_id)`;
/// `existing_by_email` = user with `profile.email` in this tenant.
pub fn link_decision(
    profile: &OAuthProfile,
    existing_by_identity: Option<UserId>,
    existing_by_email: Option<ExistingAccount>,
) -> LinkDecision {
    if let Some(user_id) = existing_by_identity {
        return LinkDecision::Login { user_id };
    }
    let has_verified_email = profile.email_verified
        && profile
            .email
            .as_deref()
            .is_some_and(|e| e.contains('@') && !e.trim().is_empty());
    if !has_verified_email {
        return LinkDecision::RejectUnverifiedEmail;
    }
    match existing_by_email {
        Some(account) => LinkDecision::LinkToExisting {
            user_id: account.user_id,
            revoke_local_credentials: !account.email_verified,
        },
        None => LinkDecision::CreateNew,
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn http_client() -> Result<reqwest::Client, OAuthError> {
    if hos_core::net::is_offline() {
        // Demo mode: no request to Google / GitHub (docs/DEMO.md).
        return Err(OAuthError::Config(
            "outbound connections are disabled on this server (demo mode)".into(),
        ));
    }
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| OAuthError::Config(e.to_string()))
}

fn map_reqwest(e: reqwest::Error) -> OAuthError {
    if e.is_timeout() {
        OAuthError::Timeout
    } else {
        OAuthError::Transport(e.without_url().to_string())
    }
}

/// Provably valid constant URL.
fn const_url(s: &'static str) -> Url {
    #[allow(clippy::expect_used)]
    Url::parse(s).expect("constant URL is valid")
}

fn normalize_email(email: Option<String>) -> Option<String> {
    email
        .map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    id_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// POSTs the authorization-code grant and parses the token response.
/// GitHub reports errors with HTTP 200 and an `error` field; both cases map to
/// [`OAuthError::TokenExchange`].
async fn token_request(
    http: &reqwest::Client,
    token_url: &Url,
    form: &[(&str, &str)],
) -> Result<TokenResponse, OAuthError> {
    let resp = http
        .post(token_url.clone())
        .header(ACCEPT, "application/json")
        .form(form)
        .send()
        .await
        .map_err(map_reqwest)?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(map_reqwest)?;
    let parsed: Option<TokenResponse> = serde_json::from_slice(&body).ok();
    match parsed {
        Some(t) if t.error.is_some() || !status.is_success() => Err(OAuthError::TokenExchange {
            status: Some(status.as_u16()),
            error: t
                .error
                .map_or_else(|| "http_error".to_owned(), |e| truncate(&e, 64)),
            description: t.error_description.map(|d| truncate(&d, 256)),
        }),
        Some(t) => Ok(t),
        None if !status.is_success() => Err(OAuthError::TokenExchange {
            status: Some(status.as_u16()),
            error: "http_error".into(),
            description: None,
        }),
        None => Err(OAuthError::InvalidResponse(
            "token response is not JSON".into(),
        )),
    }
}

async fn get_json<T: DeserializeOwned>(
    http: &reqwest::Client,
    url: Url,
    bearer: Option<&str>,
    accept: &str,
) -> Result<T, OAuthError> {
    let mut req = http.get(url).header(ACCEPT, accept);
    if let Some(token) = bearer {
        req = req.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    let resp = req.send().await.map_err(map_reqwest)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(OAuthError::Api {
            status: status.as_u16(),
        });
    }
    let body = resp.bytes().await.map_err(map_reqwest)?;
    serde_json::from_slice(&body).map_err(|e| OAuthError::InvalidResponse(e.to_string()))
}

// ---------------------------------------------------------------------------
// Google (OpenID Connect)
// ---------------------------------------------------------------------------

/// Google endpoints; override in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleEndpoints {
    /// Authorization endpoint.
    pub authorize: Url,
    /// Token endpoint.
    pub token: Url,
    /// JWKS (`jwks_uri`).
    pub jwks: Url,
    /// Accepted `iss` values.
    pub issuers: Vec<String>,
}

impl Default for GoogleEndpoints {
    /// Values from `https://accounts.google.com/.well-known/openid-configuration`.
    fn default() -> Self {
        Self {
            authorize: const_url("https://accounts.google.com/o/oauth2/v2/auth"),
            token: const_url("https://oauth2.googleapis.com/token"),
            jwks: const_url("https://www.googleapis.com/oauth2/v3/certs"),
            issuers: vec![
                "https://accounts.google.com".into(),
                "accounts.google.com".into(),
            ],
        }
    }
}

#[derive(Clone)]
struct RsaKey {
    n: Vec<u8>,
    e: Vec<u8>,
}

struct JwksCache {
    keys: HashMap<String, RsaKey>,
    fetched_at: Instant,
}

/// Google sign-in. See the [module docs](self) for the verification rules.
pub struct GoogleProvider {
    app: OAuthAppConfig,
    endpoints: GoogleEndpoints,
    http: reqwest::Client,
    jwks: RwLock<Option<JwksCache>>,
}

impl std::fmt::Debug for GoogleProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleProvider")
            .field("client_id", &self.app.client_id)
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct JwtHeader {
    alg: String,
    kid: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BoolOrString {
    Bool(bool),
    Str(String),
}

#[derive(Deserialize)]
struct IdTokenClaims {
    iss: String,
    sub: String,
    aud: Audience,
    azp: Option<String>,
    exp: i64,
    iat: Option<i64>,
    nonce: Option<String>,
    email: Option<String>,
    email_verified: Option<BoolOrString>,
    name: Option<String>,
    picture: Option<String>,
}

#[derive(Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    alg: Option<String>,
    #[serde(rename = "use")]
    use_: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

impl GoogleProvider {
    /// Provider with the real Google endpoints.
    pub fn new(app: OAuthAppConfig) -> Result<Self, OAuthError> {
        Self::with_endpoints(app, GoogleEndpoints::default())
    }

    /// Provider with custom endpoints (tests).
    pub fn with_endpoints(
        app: OAuthAppConfig,
        endpoints: GoogleEndpoints,
    ) -> Result<Self, OAuthError> {
        if app.client_id.trim().is_empty() || app.client_secret.is_empty() {
            return Err(OAuthError::Config("Google client id/secret missing".into()));
        }
        Ok(Self {
            app,
            endpoints,
            http: http_client()?,
            jwks: RwLock::new(None),
        })
    }

    fn cached_key(&self, kid: &str) -> (Option<RsaKey>, bool) {
        let guard = self.jwks.read().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(cache) => {
                let age = cache.fetched_at.elapsed();
                let key = (age < JWKS_TTL)
                    .then(|| cache.keys.get(kid).cloned())
                    .flatten();
                (key, age < JWKS_MIN_REFRESH)
            }
            None => (None, false),
        }
    }

    async fn signing_key(&self, kid: &str) -> Result<RsaKey, OAuthError> {
        let (cached, recently_fetched) = self.cached_key(kid);
        if let Some(key) = cached {
            return Ok(key);
        }
        if recently_fetched {
            return Err(OAuthError::InvalidIdToken("unknown signing key".into()));
        }
        let set: JwkSet = get_json(
            &self.http,
            self.endpoints.jwks.clone(),
            None,
            "application/json",
        )
        .await?;
        let keys: HashMap<String, RsaKey> = set
            .keys
            .into_iter()
            .filter(|k| {
                k.kty == "RSA"
                    && k.use_.as_deref().is_none_or(|u| u == "sig")
                    && k.alg.as_deref().is_none_or(|a| a == "RS256")
            })
            .filter_map(|k| {
                Some((
                    k.kid?,
                    RsaKey {
                        n: b64url_decode(&k.n?)?,
                        e: b64url_decode(&k.e?)?,
                    },
                ))
            })
            .collect();
        let found = keys.get(kid).cloned();
        *self.jwks.write().unwrap_or_else(|e| e.into_inner()) = Some(JwksCache {
            keys,
            fetched_at: Instant::now(),
        });
        found.ok_or_else(|| OAuthError::InvalidIdToken("unknown signing key".into()))
    }

    /// Verifies signature and claims of `id_token` for `pending`.
    async fn verify_id_token(
        &self,
        id_token: &str,
        pending: &PendingAuth,
        now: DateTime<Utc>,
    ) -> Result<IdTokenClaims, OAuthError> {
        let bad = |m: &str| OAuthError::InvalidIdToken(m.to_owned());
        let mut parts = id_token.split('.');
        let (Some(h64), Some(p64), Some(s64), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(bad("not a JWS compact token"));
        };
        let header: JwtHeader = b64url_decode(h64)
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(|| bad("malformed header"))?;
        // Only RS256: rejects `none`, HS256 key-confusion and anything unexpected.
        if header.alg != "RS256" {
            return Err(bad("unsupported signing algorithm"));
        }
        let kid = header.kid.ok_or_else(|| bad("missing kid"))?;
        let signature = b64url_decode(s64).ok_or_else(|| bad("malformed signature"))?;
        let key = self.signing_key(&kid).await?;
        let signed = format!("{h64}.{p64}");
        ring::signature::RsaPublicKeyComponents {
            n: key.n.as_slice(),
            e: key.e.as_slice(),
        }
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            signed.as_bytes(),
            &signature,
        )
        .map_err(|_| bad("signature verification failed"))?;

        let claims: IdTokenClaims = b64url_decode(p64)
            .and_then(|b| serde_json::from_slice(&b).ok())
            .ok_or_else(|| bad("malformed claims"))?;
        if !self.endpoints.issuers.iter().any(|i| i == &claims.iss) {
            return Err(bad("wrong issuer"));
        }
        let client_id = self.app.client_id.as_str();
        let aud_ok = match &claims.aud {
            Audience::One(a) => a == client_id,
            Audience::Many(list) => {
                list.iter().any(|a| a == client_id)
                    && (list.len() == 1 || claims.azp.as_deref() == Some(client_id))
            }
        };
        if !aud_ok || claims.azp.as_deref().is_some_and(|azp| azp != client_id) {
            return Err(bad("wrong audience"));
        }
        let now_ts = now.timestamp();
        if now_ts > claims.exp + LEEWAY_SECS {
            return Err(bad("expired"));
        }
        if claims.iat.is_some_and(|iat| iat > now_ts + LEEWAY_SECS) {
            return Err(bad("issued in the future"));
        }
        let nonce_ok = claims
            .nonce
            .as_deref()
            .is_some_and(|n| constant_time_eq(n.as_bytes(), pending.nonce.as_bytes()));
        if !nonce_ok {
            return Err(bad("nonce mismatch"));
        }
        if claims.sub.trim().is_empty() {
            return Err(bad("missing subject"));
        }
        Ok(claims)
    }
}

#[async_trait]
impl OAuthProvider for GoogleProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Google
    }

    fn authorize_url(&self, redirect_uri: &Url) -> (Url, PendingAuth) {
        let pending = PendingAuth::new(ProviderKind::Google, Utc::now());
        let mut url = self.endpoints.authorize.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.app.client_id)
            .append_pair("redirect_uri", redirect_uri.as_str())
            .append_pair("scope", "openid email profile")
            .append_pair("state", &pending.state)
            .append_pair("nonce", &pending.nonce)
            .append_pair("code_challenge", &pending.pkce_challenge())
            .append_pair("code_challenge_method", "S256")
            .append_pair("prompt", "select_account");
        (url, pending)
    }

    async fn exchange(
        &self,
        code: &str,
        pending: &PendingAuth,
        redirect_uri: &Url,
    ) -> Result<OAuthProfile, OAuthError> {
        if pending.provider != ProviderKind::Google {
            return Err(OAuthError::ProviderMismatch {
                expected: ProviderKind::Google,
                got: pending.provider,
            });
        }
        let tokens = token_request(
            &self.http,
            &self.endpoints.token,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri.as_str()),
                ("client_id", &self.app.client_id),
                ("client_secret", self.app.client_secret.expose_str()),
                ("code_verifier", pending.pkce_verifier.expose_str()),
            ],
        )
        .await?;
        let id_token = tokens
            .id_token
            .ok_or_else(|| OAuthError::InvalidResponse("token response has no id_token".into()))?;
        let claims = self.verify_id_token(&id_token, pending, Utc::now()).await?;
        let email_verified = match claims.email_verified {
            Some(BoolOrString::Bool(b)) => b,
            Some(BoolOrString::Str(s)) => s.eq_ignore_ascii_case("true"),
            None => false,
        };
        Ok(OAuthProfile {
            provider: ProviderKind::Google,
            provider_user_id: claims.sub,
            email: normalize_email(claims.email),
            email_verified,
            name: claims.name.filter(|n| !n.trim().is_empty()),
            avatar_url: claims.picture,
        })
    }
}

// ---------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------

/// GitHub endpoints; override in tests (or for GitHub Enterprise Server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubEndpoints {
    /// Authorization endpoint.
    pub authorize: Url,
    /// Token endpoint.
    pub token: Url,
    /// REST API base (`https://api.github.com`).
    pub api_base: Url,
}

impl Default for GitHubEndpoints {
    fn default() -> Self {
        Self {
            authorize: const_url("https://github.com/login/oauth/authorize"),
            token: const_url("https://github.com/login/oauth/access_token"),
            api_base: const_url("https://api.github.com"),
        }
    }
}

/// GitHub sign-in (OAuth App or GitHub App user authorization).
pub struct GitHubProvider {
    app: OAuthAppConfig,
    endpoints: GitHubEndpoints,
    http: reqwest::Client,
}

impl std::fmt::Debug for GitHubProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubProvider")
            .field("client_id", &self.app.client_id)
            .field("endpoints", &self.endpoints)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct GitHubUser {
    id: u64,
    login: String,
    name: Option<String>,
    email: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct GitHubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

const GITHUB_ACCEPT: &str = "application/vnd.github+json";

impl GitHubProvider {
    /// Provider with the real GitHub endpoints.
    pub fn new(app: OAuthAppConfig) -> Result<Self, OAuthError> {
        Self::with_endpoints(app, GitHubEndpoints::default())
    }

    /// Provider with custom endpoints.
    pub fn with_endpoints(
        app: OAuthAppConfig,
        endpoints: GitHubEndpoints,
    ) -> Result<Self, OAuthError> {
        if app.client_id.trim().is_empty() || app.client_secret.is_empty() {
            return Err(OAuthError::Config("GitHub client id/secret missing".into()));
        }
        Ok(Self {
            app,
            endpoints,
            http: http_client()?,
        })
    }

    fn api_url(&self, path: &str) -> Result<Url, OAuthError> {
        Url::parse(&format!(
            "{}/{path}",
            self.endpoints.api_base.as_str().trim_end_matches('/')
        ))
        .map_err(|e| OAuthError::Config(e.to_string()))
    }

    async fn api_get<T: DeserializeOwned>(&self, path: &str, token: &str) -> Result<T, OAuthError> {
        let url = self.api_url(path)?;
        let resp = self
            .http
            .get(url)
            .header(ACCEPT, GITHUB_ACCEPT)
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .await
            .map_err(map_reqwest)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(OAuthError::Api {
                status: status.as_u16(),
            });
        }
        let body = resp.bytes().await.map_err(map_reqwest)?;
        serde_json::from_slice(&body).map_err(|e| OAuthError::InvalidResponse(e.to_string()))
    }
}

#[async_trait]
impl OAuthProvider for GitHubProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::GitHub
    }

    fn authorize_url(&self, redirect_uri: &Url) -> (Url, PendingAuth) {
        let pending = PendingAuth::new(ProviderKind::GitHub, Utc::now());
        let mut url = self.endpoints.authorize.clone();
        url.query_pairs_mut()
            .append_pair("client_id", &self.app.client_id)
            .append_pair("redirect_uri", redirect_uri.as_str())
            .append_pair("scope", "read:user user:email")
            .append_pair("state", &pending.state)
            .append_pair("code_challenge", &pending.pkce_challenge())
            .append_pair("code_challenge_method", "S256")
            .append_pair("allow_signup", "true");
        (url, pending)
    }

    async fn exchange(
        &self,
        code: &str,
        pending: &PendingAuth,
        redirect_uri: &Url,
    ) -> Result<OAuthProfile, OAuthError> {
        if pending.provider != ProviderKind::GitHub {
            return Err(OAuthError::ProviderMismatch {
                expected: ProviderKind::GitHub,
                got: pending.provider,
            });
        }
        let tokens = token_request(
            &self.http,
            &self.endpoints.token,
            &[
                ("client_id", &self.app.client_id),
                ("client_secret", self.app.client_secret.expose_str()),
                ("code", code),
                ("redirect_uri", redirect_uri.as_str()),
                ("code_verifier", pending.pkce_verifier.expose_str()),
            ],
        )
        .await?;
        let access_token = tokens
            .access_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                OAuthError::InvalidResponse("token response has no access_token".into())
            })?;

        let user: GitHubUser = self.api_get("user", &access_token).await?;
        let (email, email_verified) = match self
            .api_get::<Vec<GitHubEmail>>("user/emails", &access_token)
            .await
        {
            Ok(emails) => emails
                .into_iter()
                .find(|e| e.primary)
                .map_or((None, false), |e| (Some(e.email), e.verified)),
            // Missing `user:email` scope / permission: fall back to the public
            // profile email, which GitHub does not vouch for → unverified.
            Err(OAuthError::Api { status: 403 | 404 }) => (user.email.clone(), false),
            Err(e) => return Err(e),
        };
        Ok(OAuthProfile {
            provider: ProviderKind::GitHub,
            provider_user_id: user.id.to_string(),
            email: normalize_email(email),
            email_verified,
            name: user
                .name
                .filter(|n| !n.trim().is_empty())
                .or(Some(user.login)),
            avatar_url: user.avatar_url,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn sb() -> SecretBox {
        SecretBox::new(&[9u8; 32])
    }

    fn profile(email: Option<&str>, verified: bool) -> OAuthProfile {
        OAuthProfile {
            provider: ProviderKind::Google,
            provider_user_id: "1".into(),
            email: email.map(str::to_owned),
            email_verified: verified,
            name: None,
            avatar_url: None,
        }
    }

    #[test]
    fn pending_seal_open_and_expiry() {
        let now = Utc::now();
        let p = PendingAuth::new(ProviderKind::GitHub, now).with_return_to("/client/services");
        let sealed = p.seal(&sb()).unwrap();
        assert!(!sealed.contains(&p.state));
        assert!(
            sealed
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        let back = PendingAuth::open(&sb(), &sealed, now + Duration::seconds(30)).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.return_to.as_deref(), Some("/client/services"));
        assert_eq!(
            PendingAuth::open(
                &sb(),
                &sealed,
                now + Duration::seconds(PENDING_TTL_SECS + 1)
            ),
            Err(OAuthError::Expired)
        );
        assert_eq!(
            PendingAuth::open(&SecretBox::new(&[1u8; 32]), &sealed, now),
            Err(OAuthError::InvalidPending)
        );
        let mut tampered = sealed.clone();
        tampered.replace_range(10..11, if &sealed[10..11] == "A" { "B" } else { "A" });
        assert_eq!(
            PendingAuth::open(&sb(), &tampered, now),
            Err(OAuthError::InvalidPending)
        );
        assert_eq!(
            PendingAuth::open(&sb(), "%%%", now),
            Err(OAuthError::InvalidPending)
        );
        assert!(!format!("{p:?}").contains(p.pkce_verifier.expose_str()));
    }

    #[test]
    fn pending_link_intent_survives_sealing() {
        let now = Utc::now();
        let user = UserId::new();
        let p = PendingAuth::new(ProviderKind::Google, now);
        assert_eq!(p.link_user_id, None);
        let p = p.with_link_user(user);
        let back = PendingAuth::open(&sb(), &p.seal(&sb()).unwrap(), now).unwrap();
        assert_eq!(back.link_user_id, Some(user));
    }

    #[test]
    fn pkce_challenge_rfc7636_vector() {
        let mut p = PendingAuth::new(ProviderKind::Google, Utc::now());
        p.pkce_verifier = SecretString::from("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            p.pkce_challenge(),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn callback_validation() {
        let now = Utc::now();
        let p = PendingAuth::new(ProviderKind::Google, now);
        let ok = CallbackParams {
            code: Some("c0de".into()),
            state: Some(p.state.clone()),
            ..CallbackParams::default()
        };
        assert_eq!(p.verify_callback(&ok, now).unwrap(), "c0de");
        let wrong_state = CallbackParams {
            state: Some("forged".into()),
            ..ok.clone()
        };
        assert_eq!(
            p.verify_callback(&wrong_state, now),
            Err(OAuthError::StateMismatch)
        );
        let no_state = CallbackParams {
            state: None,
            ..ok.clone()
        };
        assert_eq!(
            p.verify_callback(&no_state, now),
            Err(OAuthError::StateMismatch)
        );
        // Forged error callback without the right state is a state mismatch.
        let forged_error = CallbackParams {
            code: None,
            state: Some("forged".into()),
            error: Some("access_denied".into()),
            error_description: None,
        };
        assert_eq!(
            p.verify_callback(&forged_error, now),
            Err(OAuthError::StateMismatch)
        );
        let denied = CallbackParams {
            state: Some(p.state.clone()),
            ..forged_error
        };
        assert!(matches!(
            p.verify_callback(&denied, now),
            Err(OAuthError::AuthorizationDenied { ref error, .. }) if error == "access_denied"
        ));
        let no_code = CallbackParams {
            code: Some("  ".into()),
            ..ok.clone()
        };
        assert_eq!(
            p.verify_callback(&no_code, now),
            Err(OAuthError::MissingCode)
        );
        assert_eq!(
            p.verify_callback(&ok, now + Duration::minutes(11)),
            Err(OAuthError::Expired)
        );
    }

    #[test]
    fn return_paths() {
        assert_eq!(safe_return_path("/client"), Some("/client"));
        assert_eq!(safe_return_path("/a?b=c#d"), Some("/a?b=c#d"));
        for bad in [
            "https://evil.com",
            "//evil.com",
            "/\\evil.com",
            "client",
            "",
            "/x\ny",
        ] {
            assert_eq!(safe_return_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn linking_policy() {
        let u1 = UserId::new();
        let u2 = UserId::new();
        let verified = profile(Some("a@example.com"), true);
        let unverified = profile(Some("a@example.com"), false);
        let no_email = profile(None, true);

        // Identity already linked: login regardless of email state.
        assert_eq!(
            link_decision(&unverified, Some(u1), None),
            LinkDecision::Login { user_id: u1 }
        );
        assert_eq!(
            link_decision(
                &verified,
                Some(u1),
                Some(ExistingAccount {
                    user_id: u2,
                    email_verified: true
                })
            ),
            LinkDecision::Login { user_id: u1 }
        );
        // Unverified / missing email never links or creates.
        let existing = ExistingAccount {
            user_id: u2,
            email_verified: true,
        };
        assert_eq!(
            link_decision(&unverified, None, Some(existing)),
            LinkDecision::RejectUnverifiedEmail
        );
        assert_eq!(
            link_decision(&unverified, None, None),
            LinkDecision::RejectUnverifiedEmail
        );
        assert_eq!(
            link_decision(&no_email, None, None),
            LinkDecision::RejectUnverifiedEmail
        );
        // Verified email.
        assert_eq!(
            link_decision(&verified, None, Some(existing)),
            LinkDecision::LinkToExisting {
                user_id: u2,
                revoke_local_credentials: false
            }
        );
        assert_eq!(
            link_decision(
                &verified,
                None,
                Some(ExistingAccount {
                    user_id: u2,
                    email_verified: false
                })
            ),
            LinkDecision::LinkToExisting {
                user_id: u2,
                revoke_local_credentials: true
            }
        );
        assert_eq!(
            link_decision(&verified, None, None),
            LinkDecision::CreateNew
        );
    }

    #[test]
    fn error_mapping() {
        assert_eq!(
            CoreError::from(OAuthError::StateMismatch).http_status(),
            400
        );
        assert_eq!(CoreError::from(OAuthError::Timeout).http_status(), 504);
        assert_eq!(
            CoreError::from(OAuthError::TokenExchange {
                status: Some(401),
                error: "invalid_client".into(),
                description: None
            })
            .http_status(),
            502
        );
        assert_eq!(
            CoreError::from(OAuthError::Config("x".into())).http_status(),
            500
        );
    }

    #[test]
    fn providers_require_credentials() {
        let empty = OAuthAppConfig {
            client_id: String::new(),
            client_secret: SecretString::from("s"),
        };
        assert!(GoogleProvider::new(empty.clone()).is_err());
        assert!(GitHubProvider::new(empty).is_err());
        let app = OAuthAppConfig {
            client_id: "id".into(),
            client_secret: SecretString::from("s3cret"),
        };
        let g = build_provider(ProviderKind::Google, app.clone()).unwrap();
        assert_eq!(g.kind(), ProviderKind::Google);
        let gh = GitHubProvider::new(app).unwrap();
        assert!(!format!("{gh:?}").contains("s3cret"));
    }
}
