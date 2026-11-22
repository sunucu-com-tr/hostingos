//! Shared helpers: temporary directories, throw-away signing keys, signed
//! manifests, release tarballs with fake binaries.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use hos_update::keys::{KeyOrigin, KeyRing, TrustedKey};
use hos_update::layout::Layout;

/// A directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "hos-update-{tag}-{}",
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(std::fs::canonicalize(&dir).expect("canonical temp dir"))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A throw-away minisign key pair.
pub struct TestKey {
    pub pk: minisign::PublicKey,
    pub sk: minisign::SecretKey,
}

impl TestKey {
    pub fn generate() -> Self {
        let minisign::KeyPair { pk, sk } =
            minisign::KeyPair::generate_unencrypted_keypair().expect("keypair");
        Self { pk, sk }
    }

    pub fn public_b64(&self) -> String {
        self.pk.to_base64()
    }

    pub fn sign(&self, data: &[u8]) -> String {
        minisign::sign(
            Some(&self.pk),
            &self.sk,
            std::io::Cursor::new(data),
            Some("hostingos test signature"),
            None,
        )
        .expect("sign")
        .to_string()
    }

    pub fn trusted(&self) -> TrustedKey {
        TrustedKey::parse(&self.public_b64(), "test", KeyOrigin::BuiltIn).expect("key")
    }

    pub fn ring(&self) -> KeyRing {
        let mut ring = KeyRing::empty();
        ring.add(self.trusted());
        ring
    }
}

pub fn target() -> String {
    hos_update::BuildInfo::current().target
}

/// A manifest JSON value for `version` with one artefact.
pub fn manifest_json(
    version: &str,
    artefact_url: &str,
    size: u64,
    sha256: &str,
) -> serde_json::Value {
    serde_json::json!({
        "schema": 1,
        "product": "hostingos",
        "version": version,
        "channel": if version.contains('-') { "beta" } else { "stable" },
        "published_at": "2026-11-01T10:00:00Z",
        "severity": "regular",
        "min_upgradable_from": "0.0.1",
        "migrations": true,
        "rollback_safe": true,
        "notes_md": format!("### Added\n- Release {version}\n\n<script>alert(1)</script>"),
        "container_image": format!("ghcr.io/example/hostingos:{version}"),
        "artifacts": [{
            "target": target(),
            "name": format!("hostingos-{version}-{}.tar.gz", target()),
            "url": artefact_url,
            "size": size,
            "sha256": sha256,
        }]
    })
}

pub fn sha256_hex(data: &[u8]) -> String {
    hos_core::crypto::sha256_hex(data)
}

/// A shell script standing in for `hostingos` (answers `version --json`).
pub fn fake_binary(version: &str, target: &str) -> String {
    format!(
        "#!/bin/sh\nif [ \"$1\" = \"version\" ]; then\n  echo '{{\"version\":\"{version}\",\"target\":\"{target}\",\"commit\":\"test\"}}'\n  exit 0\nfi\nexit 0\n"
    )
}

/// A release tarball: `hostingos-<v>-<target>/{hostingos,static/app.css,locales/en/common.json}`.
pub fn release_tarball(version: &str, binary: &str) -> Vec<u8> {
    let top = format!("hostingos-{version}-{}", target());
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    let mut add = |path: String, data: &[u8], mode: u32| {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        h.set_entry_type(tar::EntryType::Regular);
        h.set_cksum();
        tar.append_data(&mut h, path, data).expect("append");
    };
    add(format!("{top}/hostingos"), binary.as_bytes(), 0o755);
    add(format!("{top}/static/app.css"), b"body{}", 0o644);
    add(format!("{top}/locales/en/common.json"), b"{}", 0o644);
    tar.into_inner().expect("tar").finish().expect("gzip")
}

/// A tarball with one hand-made entry (path traversal, links …).
pub fn evil_tarball(path: &str, kind: tar::EntryType) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(kind);
    h.set_size(4);
    h.set_mode(0o644);
    if kind == tar::EntryType::Symlink {
        h.set_size(0);
        h.set_link_name("/etc/passwd").expect("link");
    }
    // Bypass the builder's own path checks: write the name bytes directly.
    let name = path.as_bytes();
    h.as_old_mut().name[..name.len()].copy_from_slice(name);
    h.set_cksum();
    let data: &[u8] = if kind == tar::EntryType::Symlink {
        b""
    } else {
        b"evil"
    };
    tar.append(&h, data).expect("append");
    tar.into_inner().expect("tar").finish().expect("gzip")
}

/// `<root>/releases/<v>/` with a fake binary, `current` → it.
pub fn install_release(root: &Path, version: &str) -> PathBuf {
    let dir = root.join("releases").join(version);
    std::fs::create_dir_all(dir.join("static")).expect("static");
    std::fs::create_dir_all(dir.join("locales")).expect("locales");
    let bin = dir.join("hostingos");
    let mut f = std::fs::File::create(&bin).expect("bin");
    f.write_all(fake_binary(version, &target()).as_bytes())
        .expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    std::fs::canonicalize(dir).expect("canonical")
}

/// A layout with release `version` current.
pub fn layout_with(root: &Path, version: &str) -> (Layout, PathBuf) {
    let release = install_release(root, version);
    let layout = Layout::new(root);
    layout.point_current(&release).expect("current");
    (layout, release)
}

/// Writes `bytes` to `path`.
pub fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write");
}
