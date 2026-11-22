//! End-to-end OAuth flows against wiremock (no real network).
//!
//! The RSA keys in `fixtures/` are throwaway test keys (PKCS#8 DER, base64),
//! generated only for these tests.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::Utc;
use hos_auth::oauth::{
    CallbackParams, ExistingAccount, GitHubEndpoints, GitHubProvider, GoogleEndpoints,
    GoogleProvider, LinkDecision, OAuthError, OAuthProvider, PendingAuth, ProviderKind, complete,
    link_decision,
};
use hos_core::config::OAuthAppConfig;
use hos_core::crypto::SecretBox;
use hos_core::ids::UserId;
use hos_core::secret::SecretString;
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair, RsaPublicKeyComponents};
use serde_json::{Value, json};
use url::Url;
use wiremock::matchers::{body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY1: &str = include_str!("fixtures/test_rsa_key_1.pk8.b64");
const KEY2: &str = include_str!("fixtures/test_rsa_key_2.pk8.b64");
const CLIENT_ID: &str = "hos-test-client.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "test-client-secret";
const ISSUER: &str = "https://accounts.google.com";

fn app() -> OAuthAppConfig {
    OAuthAppConfig {
        client_id: CLIENT_ID.into(),
        client_secret: SecretString::from(CLIENT_SECRET),
    }
}

fn key_pair(b64: &str) -> RsaKeyPair {
    let der = STANDARD.decode(b64.trim()).unwrap();
    RsaKeyPair::from_pkcs8(&der).unwrap()
}

fn jwks(b64: &str, kid: &str) -> Value {
    let kp = key_pair(b64);
    let public = RsaPublicKeyComponents::<Vec<u8>>::from(kp.public());
    json!({"keys": [
        // A non-RSA key that must be ignored.
        {"kty": "EC", "kid": "ec-key", "crv": "P-256", "x": "AA", "y": "AA"},
        {"kty": "RSA", "kid": kid, "alg": "RS256", "use": "sig",
         "n": URL_SAFE_NO_PAD.encode(&public.n), "e": URL_SAFE_NO_PAD.encode(&public.e)}
    ]})
}

fn sign_jwt(b64: &str, header: &Value, claims: &Value) -> String {
    let h = URL_SAFE_NO_PAD.encode(serde_json::to_vec(header).unwrap());
    let p = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
    let signing_input = format!("{h}.{p}");
    let kp = key_pair(b64);
    let mut sig = vec![0u8; kp.public().modulus_len()];
    kp.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        signing_input.as_bytes(),
        &mut sig,
    )
    .unwrap();
    format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig))
}

fn rs256(kid: &str) -> Value {
    json!({"alg": "RS256", "kid": kid, "typ": "JWT"})
}

fn claims(nonce: &str) -> Value {
    let now = Utc::now().timestamp();
    json!({
        "iss": ISSUER,
        "azp": CLIENT_ID,
        "aud": CLIENT_ID,
        "sub": "110169484474386276334",
        "email": "Ayse.Yilmaz@Example.com",
        "email_verified": true,
        "name": "Ayşe Yılmaz",
        "picture": "https://lh3.googleusercontent.com/a/photo.jpg",
        "nonce": nonce,
        "iat": now,
        "exp": now + 3600,
    })
}

fn redirect() -> Url {
    Url::parse("https://acme.example.com/auth/oauth/google/callback").unwrap()
}

struct GoogleHarness {
    server: MockServer,
    provider: GoogleProvider,
    pending: PendingAuth,
    params: CallbackParams,
}

/// Starts a mock server, runs `authorize_url`, round-trips the pending cookie
/// and serves the JWKS for KEY1/"kid-1".
async fn google() -> GoogleHarness {
    let server = MockServer::start().await;
    let base = server.uri();
    let provider = GoogleProvider::with_endpoints(
        app(),
        GoogleEndpoints {
            authorize: Url::parse(&format!("{base}/o/oauth2/v2/auth")).unwrap(),
            token: Url::parse(&format!("{base}/token")).unwrap(),
            jwks: Url::parse(&format!("{base}/oauth2/v3/certs")).unwrap(),
            issuers: vec![ISSUER.into(), "accounts.google.com".into()],
        },
    )
    .unwrap();
    let (auth_url, pending) = provider.authorize_url(&redirect());

    let q: std::collections::HashMap<String, String> =
        auth_url.query_pairs().into_owned().collect();
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["redirect_uri"], redirect().as_str());
    assert_eq!(q["scope"], "openid email profile");
    assert_eq!(q["state"], pending.state);
    assert_eq!(q["nonce"], pending.nonce);
    assert_eq!(q["code_challenge"], pending.pkce_challenge());
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(
        !auth_url
            .as_str()
            .contains(pending.pkce_verifier.expose_str())
    );
    assert!(!auth_url.as_str().contains(CLIENT_SECRET));

    // The pending data travels through an encrypted cookie.
    let sb = SecretBox::new(&[3u8; 32]);
    let cookie = pending.seal(&sb).unwrap();
    let pending = PendingAuth::open(&sb, &cookie, Utc::now()).unwrap();

    Mock::given(method("GET"))
        .and(path("/oauth2/v3/certs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(jwks(KEY1, "kid-1")))
        .mount(&server)
        .await;

    let params = CallbackParams {
        code: Some("4/0AY0e-g7-test-code".into()),
        state: Some(pending.state.clone()),
        ..CallbackParams::default()
    };
    GoogleHarness {
        server,
        provider,
        pending,
        params,
    }
}

async fn mount_token(server: &MockServer, id_token: &str) {
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.test", "expires_in": 3599, "token_type": "Bearer",
            "scope": "openid email profile", "id_token": id_token
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn google_happy_path_with_pkce_and_jwks_cache() {
    let h = google().await;
    let id_token = sign_jwt(KEY1, &rs256("kid-1"), &claims(&h.pending.nonce));
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(header("accept", "application/json"))
        .and(body_string_contains("grant_type=authorization_code"))
        .and(body_string_contains("code=4%2F0AY0e-g7-test-code"))
        .and(body_string_contains(format!(
            "code_verifier={}",
            h.pending.pkce_verifier.expose_str()
        )))
        .and(body_string_contains(format!(
            "client_secret={CLIENT_SECRET}"
        )))
        .and(body_string_contains(
            "redirect_uri=https%3A%2F%2Facme.example.com%2Fauth%2Foauth%2Fgoogle%2Fcallback",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "ya29.test", "token_type": "Bearer", "id_token": id_token
        })))
        .expect(2)
        .mount(&h.server)
        .await;

    let profile = complete(&h.provider, &h.params, &h.pending, &redirect(), Utc::now())
        .await
        .unwrap();
    assert_eq!(profile.provider, ProviderKind::Google);
    assert_eq!(profile.provider_user_id, "110169484474386276334");
    assert_eq!(profile.email.as_deref(), Some("ayse.yilmaz@example.com"));
    assert!(profile.email_verified);
    assert_eq!(profile.name.as_deref(), Some("Ayşe Yılmaz"));
    assert!(profile.avatar_url.is_some());
    assert_eq!(link_decision(&profile, None, None), LinkDecision::CreateNew);

    // Second exchange reuses the cached JWKS (only one JWKS request in total).
    h.provider
        .exchange("4/0AY0e-g7-test-code", &h.pending, &redirect())
        .await
        .unwrap();
    let jwks_calls = h
        .server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/oauth2/v3/certs")
        .count();
    assert_eq!(jwks_calls, 1);
}

#[tokio::test]
async fn google_state_mismatch_never_calls_token_endpoint() {
    let h = google().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&h.server)
        .await;
    let forged = CallbackParams {
        state: Some("attacker-state".into()),
        ..h.params.clone()
    };
    let err = complete(&h.provider, &forged, &h.pending, &redirect(), Utc::now())
        .await
        .unwrap_err();
    assert_eq!(err, OAuthError::StateMismatch);

    let denied = CallbackParams {
        code: None,
        error: Some("access_denied".into()),
        ..h.params.clone()
    };
    let err = complete(&h.provider, &denied, &h.pending, &redirect(), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(err, OAuthError::AuthorizationDenied { .. }));
}

#[tokio::test]
async fn google_unverified_email_is_not_linked() {
    let h = google().await;
    let mut c = claims(&h.pending.nonce);
    c["email_verified"] = json!("false");
    mount_token(&h.server, &sign_jwt(KEY1, &rs256("kid-1"), &c)).await;
    let profile = complete(&h.provider, &h.params, &h.pending, &redirect(), Utc::now())
        .await
        .unwrap();
    assert!(!profile.email_verified);
    let existing = ExistingAccount {
        user_id: UserId::new(),
        email_verified: true,
    };
    assert_eq!(
        link_decision(&profile, None, Some(existing)),
        LinkDecision::RejectUnverifiedEmail
    );
}

async fn google_rejects(mutate: impl FnOnce(&mut Value, &mut Value) -> &'static str) {
    let h = google().await;
    let mut hdr = rs256("kid-1");
    let mut c = claims(&h.pending.nonce);
    let key = mutate(&mut hdr, &mut c);
    mount_token(&h.server, &sign_jwt(key, &hdr, &c)).await;
    let err = complete(&h.provider, &h.params, &h.pending, &redirect(), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(err, OAuthError::InvalidIdToken(_)), "{err:?}");
}

#[tokio::test]
async fn google_rejects_nonce_mismatch() {
    google_rejects(|_, c| {
        c["nonce"] = json!("replayed-nonce");
        KEY1
    })
    .await;
}

#[tokio::test]
async fn google_rejects_missing_nonce() {
    google_rejects(|_, c| {
        c.as_object_mut().unwrap().remove("nonce");
        KEY1
    })
    .await;
}

#[tokio::test]
async fn google_rejects_wrong_audience() {
    google_rejects(|_, c| {
        c["aud"] = json!("someone-else.apps.googleusercontent.com");
        c["azp"] = json!("someone-else.apps.googleusercontent.com");
        KEY1
    })
    .await;
}

#[tokio::test]
async fn google_rejects_multi_audience_without_matching_azp() {
    google_rejects(|_, c| {
        c["aud"] = json!([CLIENT_ID, "other-client"]);
        c["azp"] = json!("other-client");
        KEY1
    })
    .await;
}

#[tokio::test]
async fn google_rejects_wrong_issuer() {
    google_rejects(|_, c| {
        c["iss"] = json!("https://evil.example.com");
        KEY1
    })
    .await;
}

#[tokio::test]
async fn google_rejects_expired_token() {
    google_rejects(|_, c| {
        let now = Utc::now().timestamp();
        c["iat"] = json!(now - 7200);
        c["exp"] = json!(now - 3600);
        KEY1
    })
    .await;
}

#[tokio::test]
async fn google_rejects_signature_from_other_key() {
    // Same kid, signed by a key that is not in the JWKS.
    google_rejects(|_, _| KEY2).await;
}

#[tokio::test]
async fn google_rejects_unknown_kid() {
    google_rejects(|h, _| {
        h["kid"] = json!("rotated-away");
        KEY1
    })
    .await;
}

#[tokio::test]
async fn google_rejects_non_rs256_algorithms() {
    google_rejects(|h, _| {
        h["alg"] = json!("HS256");
        KEY1
    })
    .await;
    // Unsigned token.
    let h = google().await;
    let hdr = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims(&h.pending.nonce)).unwrap());
    mount_token(&h.server, &format!("{hdr}.{body}.")).await;
    let err = complete(&h.provider, &h.params, &h.pending, &redirect(), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(err, OAuthError::InvalidIdToken(_)), "{err:?}");
}

#[tokio::test]
async fn google_rejects_tampered_payload() {
    let h = google().await;
    let token = sign_jwt(KEY1, &rs256("kid-1"), &claims(&h.pending.nonce));
    let mut parts: Vec<&str> = token.split('.').collect();
    let mut forged_claims = claims(&h.pending.nonce);
    forged_claims["email"] = json!("ceo@victim.example");
    let forged = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged_claims).unwrap());
    parts[1] = &forged;
    mount_token(&h.server, &parts.join(".")).await;
    let err = complete(&h.provider, &h.params, &h.pending, &redirect(), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(err, OAuthError::InvalidIdToken(_)), "{err:?}");
}

#[tokio::test]
async fn google_token_endpoint_errors() {
    let h = google().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant", "error_description": "Bad Request"
        })))
        .mount(&h.server)
        .await;
    let err = complete(&h.provider, &h.params, &h.pending, &redirect(), Utc::now())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        OAuthError::TokenExchange {
            status: Some(400),
            error: "invalid_grant".into(),
            description: Some("Bad Request".into()),
        }
    );
    assert!(!err.to_string().contains(CLIENT_SECRET));

    // Non-JSON 5xx.
    let h = google().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(503).set_body_string("<html>down</html>"))
        .mount(&h.server)
        .await;
    let err = complete(&h.provider, &h.params, &h.pending, &redirect(), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        OAuthError::TokenExchange {
            status: Some(503),
            ..
        }
    ));

    // Success response without id_token.
    let h = google().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"access_token": "x"})))
        .mount(&h.server)
        .await;
    let err = complete(&h.provider, &h.params, &h.pending, &redirect(), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(err, OAuthError::InvalidResponse(_)));
}

#[tokio::test]
async fn pending_for_other_provider_is_rejected() {
    let h = google().await;
    let mut gh_pending = PendingAuth::new(ProviderKind::GitHub, Utc::now());
    gh_pending.state = h.pending.state.clone();
    let err = complete(&h.provider, &h.params, &gh_pending, &redirect(), Utc::now())
        .await
        .unwrap_err();
    assert!(matches!(err, OAuthError::ProviderMismatch { .. }));
}

// ---------------------------------------------------------------------------
// GitHub
// ---------------------------------------------------------------------------

struct GitHubHarness {
    server: MockServer,
    provider: GitHubProvider,
    pending: PendingAuth,
    params: CallbackParams,
}

fn gh_redirect() -> Url {
    Url::parse("https://acme.example.com/auth/oauth/github/callback").unwrap()
}

async fn github() -> GitHubHarness {
    let server = MockServer::start().await;
    let base = server.uri();
    let provider = GitHubProvider::with_endpoints(
        app(),
        GitHubEndpoints {
            authorize: Url::parse(&format!("{base}/login/oauth/authorize")).unwrap(),
            token: Url::parse(&format!("{base}/login/oauth/access_token")).unwrap(),
            api_base: Url::parse(&format!("{base}/api")).unwrap(),
        },
    )
    .unwrap();
    let (auth_url, pending) = provider.authorize_url(&gh_redirect());
    let q: std::collections::HashMap<String, String> =
        auth_url.query_pairs().into_owned().collect();
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["scope"], "read:user user:email");
    assert_eq!(q["state"], pending.state);
    assert_eq!(q["code_challenge_method"], "S256");
    assert!(!q.contains_key("nonce"));
    let params = CallbackParams {
        code: Some("gh-code-1".into()),
        state: Some(pending.state.clone()),
        ..CallbackParams::default()
    };
    GitHubHarness {
        server,
        provider,
        pending,
        params,
    }
}

async fn mount_github_token(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(header("accept", "application/json"))
        .and(body_string_contains("code=gh-code-1"))
        .and(body_string_contains(format!("client_id={CLIENT_ID}")))
        .and(body_string_contains("code_verifier="))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "gho_test_token", "token_type": "bearer", "scope": "read:user,user:email"
        })))
        .mount(server)
        .await;
}

async fn mount_github_user(server: &MockServer, public_email: Value) {
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .and(header("authorization", "Bearer gho_test_token"))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "login": "octocat", "id": 583231, "name": null,
            "email": public_email, "avatar_url": "https://avatars.githubusercontent.com/u/583231"
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn github_happy_path_uses_verified_primary_email() {
    let h = github().await;
    mount_github_token(&h.server).await;
    mount_github_user(&h.server, json!("public@example.com")).await;
    Mock::given(method("GET"))
        .and(path("/api/user/emails"))
        .and(header("authorization", "Bearer gho_test_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"email": "secondary@example.com", "primary": false, "verified": true, "visibility": null},
            {"email": "Octo@Example.com", "primary": true, "verified": true, "visibility": "private"}
        ])))
        .mount(&h.server)
        .await;
    let profile = complete(
        &h.provider,
        &h.params,
        &h.pending,
        &gh_redirect(),
        Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(profile.provider, ProviderKind::GitHub);
    assert_eq!(profile.provider_user_id, "583231");
    assert_eq!(profile.email.as_deref(), Some("octo@example.com"));
    assert!(profile.email_verified);
    assert_eq!(profile.name.as_deref(), Some("octocat"));
    let existing = ExistingAccount {
        user_id: UserId::new(),
        email_verified: false,
    };
    assert!(matches!(
        link_decision(&profile, None, Some(existing)),
        LinkDecision::LinkToExisting {
            revoke_local_credentials: true,
            ..
        }
    ));
}

#[tokio::test]
async fn github_unverified_primary_email() {
    let h = github().await;
    mount_github_token(&h.server).await;
    mount_github_user(&h.server, Value::Null).await;
    Mock::given(method("GET"))
        .and(path("/api/user/emails"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"email": "new@example.com", "primary": true, "verified": false, "visibility": "private"},
            {"email": "other@example.com", "primary": false, "verified": true, "visibility": null}
        ])))
        .mount(&h.server)
        .await;
    let profile = complete(
        &h.provider,
        &h.params,
        &h.pending,
        &gh_redirect(),
        Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(profile.email.as_deref(), Some("new@example.com"));
    assert!(!profile.email_verified);
    assert_eq!(
        link_decision(&profile, None, None),
        LinkDecision::RejectUnverifiedEmail
    );
    // An already-linked identity still logs in.
    let uid = UserId::new();
    assert_eq!(
        link_decision(&profile, Some(uid), None),
        LinkDecision::Login { user_id: uid }
    );
}

#[tokio::test]
async fn github_emails_forbidden_falls_back_to_unverified_public_email() {
    let h = github().await;
    mount_github_token(&h.server).await;
    mount_github_user(&h.server, json!("public@example.com")).await;
    Mock::given(method("GET"))
        .and(path("/api/user/emails"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"message": "Not Found"})))
        .mount(&h.server)
        .await;
    let profile = complete(
        &h.provider,
        &h.params,
        &h.pending,
        &gh_redirect(),
        Utc::now(),
    )
    .await
    .unwrap();
    assert_eq!(profile.email.as_deref(), Some("public@example.com"));
    assert!(!profile.email_verified);
}

#[tokio::test]
async fn github_token_error_in_200_body() {
    let h = github().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "error": "bad_verification_code",
            "error_description": "The code passed is incorrect or expired.",
            "error_uri": "https://docs.github.com/apps/troubleshooting"
        })))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&h.server)
        .await;
    let err = complete(
        &h.provider,
        &h.params,
        &h.pending,
        &gh_redirect(),
        Utc::now(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        OAuthError::TokenExchange { ref error, status: Some(200), .. } if error == "bad_verification_code"
    ));
}

#[tokio::test]
async fn github_user_api_unauthorized() {
    let h = github().await;
    mount_github_token(&h.server).await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})),
        )
        .mount(&h.server)
        .await;
    let err = complete(
        &h.provider,
        &h.params,
        &h.pending,
        &gh_redirect(),
        Utc::now(),
    )
    .await
    .unwrap_err();
    assert_eq!(err, OAuthError::Api { status: 401 });
    assert_eq!(hos_core::CoreError::from(err).http_status(), 502);
}

#[tokio::test]
async fn github_state_mismatch() {
    let h = github().await;
    let forged = CallbackParams {
        state: Some(h.pending.state.to_uppercase()),
        ..h.params.clone()
    };
    let err = complete(&h.provider, &forged, &h.pending, &gh_redirect(), Utc::now())
        .await
        .unwrap_err();
    assert_eq!(err, OAuthError::StateMismatch);
    assert!(h.server.received_requests().await.unwrap().is_empty());
}
