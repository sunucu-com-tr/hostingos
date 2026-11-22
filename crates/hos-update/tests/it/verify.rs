//! Manifest and artefact signatures: valid, tampered, wrong key, rotated
//! key, downgrade, replay, wrong target, oversized.

use chrono::{TimeZone as _, Utc};
use hos_update::keys::{KeyOrigin, KeyRing, TrustedKey, signature_info};
use hos_update::manifest::{Channel, MAX_MANIFEST_BYTES};
use hos_update::verify::{VerifyPolicy, verify_manifest, verify_signed};
use hos_update::{Manifest, UpdateError};

use crate::support::{TempDir, TestKey, manifest_json, sha256_hex, target};

fn policy(current: &str) -> VerifyPolicy {
    VerifyPolicy {
        current: semver::Version::parse(current).unwrap(),
        channel: Channel::Stable,
        now: Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).unwrap(),
        seen_published_at: None,
    }
}

fn signed(key: &TestKey, json: &serde_json::Value) -> (Vec<u8>, String) {
    let raw = serde_json::to_vec_pretty(json).unwrap();
    let sig = key.sign(&raw);
    (raw, sig)
}

fn sample(version: &str) -> serde_json::Value {
    manifest_json(
        version,
        "https://github.com/x/y/a.tar.gz",
        10,
        &"ab".repeat(32),
    )
}

#[test]
fn valid_manifest_verifies() {
    let key = TestKey::generate();
    let (raw, sig) = signed(&key, &sample("0.2.0"));
    let info = signature_info(&sig).unwrap();
    assert!(info.prehashed, "minisign signs pre-hashed by default");
    assert_eq!(info.key_id, key.trusted().key_id);
    let v = verify_manifest(&raw, &sig, &key.ring(), &policy("0.1.0")).expect("verifies");
    assert_eq!(v.manifest.version, "0.2.0");
    assert_eq!(v.key_id, key.trusted().key_id);
    assert!(v.manifest.artifact_for(&target()).is_ok());
}

#[test]
fn tampered_manifest_is_refused() {
    let key = TestKey::generate();
    let (mut raw, sig) = signed(&key, &sample("0.2.0"));
    // Change one byte of the signed document.
    let pos = raw.iter().position(|b| *b == b'2').unwrap();
    raw[pos] = b'9';
    let err = verify_manifest(&raw, &sig, &key.ring(), &policy("0.1.0")).unwrap_err();
    assert!(matches!(err, UpdateError::Signature(_)), "{err}");

    // A tampered trusted comment breaks the global signature.
    let (raw, sig) = signed(&key, &sample("0.2.0"));
    let forged = sig.replace("hostingos test signature", "hostingos evil signature");
    assert_ne!(forged, sig);
    let err = verify_signed(&raw, &forged, &key.ring()).unwrap_err();
    assert!(matches!(err, UpdateError::Signature(_)), "{err}");
}

#[test]
fn wrong_key_and_no_key_are_refused() {
    let signer = TestKey::generate();
    let other = TestKey::generate();
    let (raw, sig) = signed(&signer, &sample("0.2.0"));
    let err = verify_signed(&raw, &sig, &other.ring()).unwrap_err();
    assert!(
        matches!(&err, UpdateError::Signature(m) if m.contains("unknown key")),
        "{err}"
    );
    let err = verify_signed(&raw, &sig, &KeyRing::empty()).unwrap_err();
    assert!(matches!(err, UpdateError::NoTrustedKeys), "{err}");
}

#[test]
fn rotated_key_needs_an_endorsement() {
    let old = TestKey::generate();
    let new = TestKey::generate();
    // Release 0.2.0, signed with the old key, announces the new one.
    let mut m = sample("0.2.0");
    m["next_keys"] = serde_json::json!([new.public_b64()]);
    let (raw, sig) = signed(&old, &m);
    let v = verify_manifest(&raw, &sig, &old.ring(), &policy("0.1.0")).unwrap();
    // Release 0.3.0 is signed with the new key only.
    let (raw3, sig3) = signed(&new, &sample("0.3.0"));
    let mut ring = old.ring();
    assert!(
        verify_signed(&raw3, &sig3, &ring).is_err(),
        "not trusted yet"
    );
    for k in &v.manifest.next_keys {
        ring.add(
            TrustedKey::parse(
                k,
                "announced",
                KeyOrigin::Endorsed {
                    by: v.key_id.clone(),
                    version: v.manifest.version.clone(),
                },
            )
            .unwrap(),
        );
    }
    let v3 = verify_manifest(&raw3, &sig3, &ring, &policy("0.2.0")).expect("endorsed key");
    assert_eq!(v3.key_id, new.trusted().key_id);
    // Endorsed keys can be revoked; built-in ones cannot.
    assert!(ring.revoke(&new.trusted().key_id));
    assert!(!ring.revoke(&old.trusted().key_id));
    assert!(verify_signed(&raw3, &sig3, &ring).is_err());
}

#[test]
fn downgrade_and_reinstall_are_refused() {
    let key = TestKey::generate();
    for (offered, current) in [("0.1.0", "0.2.0"), ("0.2.0", "0.2.0")] {
        let (raw, sig) = signed(&key, &sample(offered));
        let err = verify_manifest(&raw, &sig, &key.ring(), &policy(current)).unwrap_err();
        assert!(matches!(err, UpdateError::NotNewer { .. }), "{err}");
    }
}

#[test]
fn replayed_and_future_manifests_are_refused() {
    let key = TestKey::generate();
    let (raw, sig) = signed(&key, &sample("0.2.0"));
    let mut p = policy("0.1.0");
    // A newer manifest for 0.2.0 was already accepted: the old one is a replay.
    p.seen_published_at = Some(Utc.with_ymd_and_hms(2026, 11, 10, 0, 0, 0).unwrap());
    let err = verify_manifest(&raw, &sig, &key.ring(), &p).unwrap_err();
    assert!(matches!(err, UpdateError::Replay(_)), "{err}");
    // The same manifest again is fine (idempotent checks).
    p.seen_published_at = Some(Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 0).unwrap());
    assert!(verify_manifest(&raw, &sig, &key.ring(), &p).is_ok());

    let mut m = sample("0.2.0");
    m["published_at"] = serde_json::json!("2027-06-01T00:00:00Z");
    let (raw, sig) = signed(&key, &m);
    let err = verify_manifest(&raw, &sig, &key.ring(), &policy("0.1.0")).unwrap_err();
    assert!(matches!(err, UpdateError::Replay(_)), "{err}");
}

#[test]
fn min_upgradable_and_channel() {
    let key = TestKey::generate();
    let mut m = sample("0.5.0");
    m["min_upgradable_from"] = serde_json::json!("0.3.0");
    let (raw, sig) = signed(&key, &m);
    let err = verify_manifest(&raw, &sig, &key.ring(), &policy("0.2.0")).unwrap_err();
    assert!(matches!(err, UpdateError::NotUpgradable { .. }), "{err}");

    let (raw, sig) = signed(&key, &sample("0.6.0-beta.1"));
    let err = verify_manifest(&raw, &sig, &key.ring(), &policy("0.2.0")).unwrap_err();
    assert!(
        matches!(err, UpdateError::Manifest(_)),
        "beta on stable: {err}"
    );
    let mut beta = policy("0.2.0");
    beta.channel = Channel::Beta;
    assert!(verify_manifest(&raw, &sig, &key.ring(), &beta).is_ok());
}

#[test]
fn wrong_target_and_oversized() {
    let key = TestKey::generate();
    let mut m = sample("0.2.0");
    m["artifacts"][0]["target"] = serde_json::json!("riscv64gc-unknown-linux-musl");
    let (raw, sig) = signed(&key, &m);
    let v = verify_manifest(&raw, &sig, &key.ring(), &policy("0.1.0")).unwrap();
    assert!(matches!(
        v.manifest.artifact_for(&target()),
        Err(UpdateError::WrongTarget(_))
    ));

    let mut big = sample("0.2.0");
    big["notes_md"] = serde_json::json!("x".repeat(MAX_MANIFEST_BYTES));
    let (raw, sig) = signed(&key, &big);
    let err = verify_manifest(&raw, &sig, &key.ring(), &policy("0.1.0")).unwrap_err();
    assert!(matches!(err, UpdateError::TooLarge { .. }), "{err}");
    let mut m = sample("0.2.0");
    m["artifacts"][0]["size"] = serde_json::json!(10_u64 * 1024 * 1024 * 1024);
    assert!(Manifest::parse(&serde_json::to_vec(&m).unwrap()).is_err());
}

#[test]
fn artefact_signature_and_tampering() {
    let key = TestKey::generate();
    let dir = TempDir::new("sig");
    let file = dir.path().join("a.tar.gz");
    let data = b"release bytes".repeat(1000);
    std::fs::write(&file, &data).unwrap();
    let sig = key.sign(&data);
    let ok = key
        .ring()
        .verify_file(&file, &sig)
        .expect("artefact verifies");
    assert_eq!(ok.key_id, key.trusted().key_id);
    assert_eq!(sha256_hex(&data).len(), 64);

    let mut tampered = data.clone();
    tampered[10] ^= 1;
    std::fs::write(&file, &tampered).unwrap();
    assert!(matches!(
        key.ring().verify_file(&file, &sig),
        Err(UpdateError::Signature(_))
    ));
}
