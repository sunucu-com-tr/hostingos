//! Downloads: redirects (allowed host / foreign host / too many), size
//! limits, checksums, HTTPS enforcement.

use hos_update::UpdateError;
use hos_update::fetch::{Fetcher, NetConfig};
use hos_update::manifest::Artifact;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{TempDir, sha256_hex};

fn fetcher() -> Fetcher {
    Fetcher::new(NetConfig::insecure_for_tests(["127.0.0.1".to_owned()])).unwrap()
}

#[tokio::test]
async fn follows_redirects_on_allowed_hosts_only() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/final"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/final"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/evil"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", "https://evil.example/payload"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/loop"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
        .mount(&server)
        .await;
    let f = fetcher();
    let base = Url::parse(&server.uri()).unwrap();
    let body = f
        .get_bytes(&base.join("/start").unwrap(), "*/*", 1024)
        .await
        .expect("redirect followed");
    assert_eq!(body, b"hello");

    let err = f
        .get_bytes(&base.join("/evil").unwrap(), "*/*", 1024)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, UpdateError::Refused(m) if m.contains("evil.example")),
        "{err}"
    );
    let err = f
        .get_bytes(&base.join("/loop").unwrap(), "*/*", 1024)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, UpdateError::Refused(m) if m.contains("too many")),
        "{err}"
    );
    // A host outside the allow-list is refused before any request.
    let err = f
        .get_bytes(&Url::parse("http://localhost:1/x").unwrap(), "*/*", 1)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Refused(_)), "{err}");
}

#[tokio::test]
async fn size_limits_and_checksums() {
    let server = MockServer::start().await;
    let data = b"0123456789".repeat(100);
    Mock::given(method("GET"))
        .and(path("/a.tar.gz"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(data.clone()))
        .mount(&server)
        .await;
    let f = fetcher();
    let url = format!("{}/a.tar.gz", server.uri());
    let err = f
        .get_bytes(&Url::parse(&url).unwrap(), "*/*", 100)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::TooLarge { .. }), "{err}");

    let dir = TempDir::new("fetch");
    let dest = dir.path().join("a.tar.gz");
    let mut art = Artifact {
        target: "t".into(),
        name: "a.tar.gz".into(),
        url: url.clone(),
        size: data.len() as u64,
        sha256: sha256_hex(&data),
        signature_url: None,
    };
    f.download(&art, &dest).await.expect("download");
    assert_eq!(std::fs::read(&dest).unwrap(), data);

    art.sha256 = sha256_hex(b"other");
    let err = f.download(&art, &dest).await.unwrap_err();
    assert!(matches!(err, UpdateError::Checksum(_)), "{err}");

    art.sha256 = sha256_hex(&data);
    art.size = 10;
    let err = f.download(&art, &dest).await.unwrap_err();
    assert!(matches!(err, UpdateError::TooLarge { .. }), "{err}");

    art.size = data.len() as u64 + 5;
    let err = f.download(&art, &dest).await.unwrap_err();
    assert!(
        matches!(err, UpdateError::Checksum(_)),
        "short download: {err}"
    );
    assert!(
        !dir.path().join("a.part").exists() && !dir.path().join("a.tar.part").exists(),
        "no partial file left behind"
    );
}

#[tokio::test]
async fn https_is_enforced_in_production() {
    let f = Fetcher::new(NetConfig::new(["example.com".to_owned()])).unwrap();
    let err = f
        .get_bytes(&Url::parse("http://example.com/x").unwrap(), "*/*", 10)
        .await
        .unwrap_err();
    assert!(matches!(err, UpdateError::Refused(_)), "{err}");
}
