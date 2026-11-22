//! The update check against a wiremock GitHub release API: discovery,
//! channels, signatures, key rotation, replay, announce-only mode.

use std::collections::HashMap;

use chrono::{TimeZone as _, Utc};
use hos_update::check::{CheckRequest, Trust, run_check};
use hos_update::fetch::{Fetcher, NetConfig};
use hos_update::keys::KeyRing;
use hos_update::manifest::Channel;
use hos_update::source::Source;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{TestKey, manifest_json};

struct Release {
    tag: &'static str,
    prerelease: bool,
    draft: bool,
    manifest: Vec<u8>,
    signature: String,
}

async fn serve(server: &MockServer, releases: &[Release]) {
    let list: Vec<serde_json::Value> = releases
        .iter()
        .map(|r| {
            serde_json::json!({
                "tag_name": r.tag,
                "draft": r.draft,
                "prerelease": r.prerelease,
                "html_url": format!("{}/releases/{}", server.uri(), r.tag),
                "assets": [
                    {"name": "hostingos-manifest.json",
                     "browser_download_url": format!("{}/dl/{}/hostingos-manifest.json", server.uri(), r.tag)},
                    {"name": "hostingos-manifest.json.minisig",
                     "browser_download_url": format!("{}/dl/{}/hostingos-manifest.json.minisig", server.uri(), r.tag)},
                ]
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/repos/acme/hostingos/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list))
        .mount(server)
        .await;
    for r in releases {
        // GitHub answers asset downloads with a redirect to its CDN.
        Mock::given(method("GET"))
            .and(path(format!("/dl/{}/hostingos-manifest.json", r.tag)))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("/cdn/{}/manifest", r.tag).as_str()),
            )
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/cdn/{}/manifest", r.tag)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(r.manifest.clone()))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/dl/{}/hostingos-manifest.json.minisig",
                r.tag
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string(r.signature.clone()))
            .mount(server)
            .await;
    }
}

fn release(tag: &'static str, key: &TestKey, json: serde_json::Value) -> Release {
    let manifest = serde_json::to_vec(&json).unwrap();
    let signature = key.sign(&manifest);
    Release {
        tag,
        prerelease: tag.contains('-'),
        draft: false,
        manifest,
        signature,
    }
}

fn manifest(version: &str) -> serde_json::Value {
    manifest_json(
        version,
        "https://github.com/acme/hostingos/a.tar.gz",
        10,
        &"cd".repeat(32),
    )
}

fn request(server: &MockServer, ring: KeyRing, current: &str) -> CheckRequest {
    CheckRequest {
        source: Source::github("acme/hostingos")
            .unwrap()
            .with_api_base(Url::parse(&server.uri()).unwrap()),
        channel: Channel::Stable,
        current: semver::Version::parse(current).unwrap(),
        ring,
        seen: HashMap::new(),
        now: Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).unwrap(),
    }
}

fn fetcher() -> Fetcher {
    Fetcher::new(NetConfig::insecure_for_tests(["127.0.0.1".to_owned()])).unwrap()
}

#[tokio::test]
async fn discovers_verifies_and_follows_key_rotation() {
    let server = MockServer::start().await;
    let old = TestKey::generate();
    let new = TestKey::generate();
    let mut m2 = manifest("0.2.0");
    m2["next_keys"] = serde_json::json!([new.public_b64()]);
    let mut m3 = manifest("0.3.0");
    m3["severity"] = serde_json::json!("security");
    let mut draft = release("v9.0.0", &old, manifest("9.0.0"));
    draft.draft = true;
    serve(
        &server,
        &[
            release("v0.3.0", &new, m3),
            release("v0.2.0", &old, m2),
            release("v0.4.0-beta.1", &old, manifest("0.4.0-beta.1")),
            release("v0.1.0", &old, manifest("0.1.0")),
            draft,
        ],
    )
    .await;

    let report = run_check(&fetcher(), &request(&server, old.ring(), "0.1.0"))
        .await
        .expect("check");
    let versions: Vec<_> = report.found.iter().map(|f| f.version.as_str()).collect();
    assert_eq!(
        versions,
        ["0.3.0", "0.2.0"],
        "stable only, newer only, no drafts"
    );
    assert!(
        report
            .found
            .iter()
            .all(|f| matches!(f.trust, Trust::Verified { .. })),
        "{:?}",
        report.found
    );
    assert_eq!(report.endorsed.len(), 1, "0.2.0 announced the new key");
    assert_eq!(report.endorsed[0].key_id, new.trusted().key_id);
    let best = report.best().expect("installable release");
    assert_eq!(best.version, "0.3.0");
    assert_eq!(
        best.manifest.as_ref().unwrap().severity,
        hos_update::Severity::Security
    );

    // Beta channel sees the pre-release too.
    let mut beta = request(&server, old.ring(), "0.1.0");
    beta.channel = Channel::Beta;
    let report = run_check(&fetcher(), &beta).await.expect("check");
    assert_eq!(report.found[0].version, "0.4.0-beta.1");
}

#[tokio::test]
async fn bad_signatures_replays_and_announce_only_mode() {
    let server = MockServer::start().await;
    let key = TestKey::generate();
    let stranger = TestKey::generate();
    serve(
        &server,
        &[
            release("v0.3.0", &stranger, manifest("0.3.0")),
            release("v0.2.0", &key, manifest("0.2.0")),
        ],
    )
    .await;

    let mut req = request(&server, key.ring(), "0.1.0");
    req.seen.insert(
        "0.2.0".into(),
        Utc.with_ymd_and_hms(2026, 11, 15, 0, 0, 0).unwrap(),
    );
    let report = run_check(&fetcher(), &req).await.expect("check");
    let by = |v: &str| report.found.iter().find(|f| f.version == v).unwrap();
    assert!(
        matches!(&by("0.3.0").trust, Trust::Rejected { reason } if reason.contains("unknown key")),
        "{:?}",
        by("0.3.0").trust
    );
    assert!(
        by("0.3.0").manifest.is_none(),
        "contents of a rejected manifest are dropped"
    );
    assert!(
        matches!(&by("0.2.0").trust, Trust::Rejected { reason } if reason.contains("older")),
        "replayed manifest: {:?}",
        by("0.2.0").trust
    );
    assert!(report.best().is_none());

    // No key compiled in: releases are announced, unverified, never "best".
    let report = run_check(&fetcher(), &request(&server, KeyRing::empty(), "0.1.0"))
        .await
        .expect("check");
    assert!(report.unsigned_mode);
    assert_eq!(report.found.len(), 2);
    assert!(report.found.iter().all(|f| f.trust == Trust::Unverified));
    assert!(report.best().is_none());
}

#[tokio::test]
async fn listing_errors_fail_the_check() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/hostingos/releases"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let err = run_check(&fetcher(), &request(&server, KeyRing::empty(), "0.1.0"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("403"), "{err}");
}
