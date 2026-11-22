//! Build metadata for `hos_update::build_info`:
//!
//! * `HOS_BUILD_COMMIT` — the git commit: the `HOS_BUILD_COMMIT` environment
//!   variable (CI passes `GITHUB_SHA`), else `git rev-parse HEAD`, else
//!   `unknown` (building from a source tarball without git is fine).
//! * `HOS_BUILD_EPOCH` — the build date as a Unix timestamp: `SOURCE_DATE_EPOCH`
//!   (reproducible builds), else the commit time, else empty. The wall clock
//!   is never used, so two builds of the same commit are identical.
//! * `HOS_BUILD_TARGET` / `HOS_BUILD_PROFILE` — target triple and profile.
//!
//! It also checks `keys/trusted.pub` (the release signing keys compiled into
//! the binary): a malformed line fails the build.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()));

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=keys/trusted.pub");
    println!("cargo:rerun-if-env-changed=HOS_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let target = std::env::var("TARGET").unwrap_or_default();
    let profile = std::env::var("PROFILE").unwrap_or_default();
    println!("cargo:rustc-env=HOS_BUILD_TARGET={target}");
    println!("cargo:rustc-env=HOS_BUILD_PROFILE={profile}");

    watch_git(&manifest_dir);
    let commit = std::env::var("HOS_BUILD_COMMIT")
        .ok()
        .map(|c| c.trim().to_ascii_lowercase())
        .filter(|c| is_commit(c))
        .or_else(|| git(&manifest_dir, &["rev-parse", "HEAD"]).filter(|c| is_commit(c)))
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=HOS_BUILD_COMMIT={commit}");

    let epoch = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .filter(|v| v.trim().parse::<i64>().is_ok())
        .map(|v| v.trim().to_owned())
        .or_else(|| {
            git(&manifest_dir, &["log", "-1", "--format=%ct"]).filter(|v| v.parse::<i64>().is_ok())
        })
        .unwrap_or_default();
    println!("cargo:rustc-env=HOS_BUILD_EPOCH={epoch}");

    check_keys(&manifest_dir.join("keys/trusted.pub"));
}

fn is_commit(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!s.is_empty()).then_some(s)
}

/// Re-run when HEAD moves (checkout, commit). Only existing files are
/// watched: a missing path would re-run the script on every build.
fn watch_git(dir: &Path) {
    let Some(git_dir) = git(dir, &["rev-parse", "--absolute-git-dir"]) else {
        return;
    };
    let git_dir = PathBuf::from(git_dir);
    let head = git_dir.join("HEAD");
    if head.is_file() {
        println!("cargo:rerun-if-changed={}", head.display());
    }
    if let Some(reference) = git(dir, &["symbolic-ref", "-q", "HEAD"]) {
        let common = git(dir, &["rev-parse", "--git-common-dir"])
            .map(|c| {
                let p = PathBuf::from(c);
                if p.is_absolute() { p } else { dir.join(p) }
            })
            .unwrap_or_else(|| git_dir.clone());
        for base in [&git_dir, &common] {
            let file = base.join(&reference);
            if file.is_file() {
                println!("cargo:rerun-if-changed={}", file.display());
            }
            let packed = base.join("packed-refs");
            if packed.is_file() {
                println!("cargo:rerun-if-changed={}", packed.display());
            }
        }
    }
}

/// Every non-comment line: `<56-char base64 minisign public key> [label]`.
fn check_keys(path: &Path) {
    // Missing: `include_str!` in src/keys.rs fails the real build; dependency
    // pre-builds (cargo-chef's skeleton) have no key file and need none.
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for (no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let key = line.split_whitespace().next().unwrap_or_default();
        let ok = key.len() == 56
            && key.starts_with("RW")
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/');
        assert!(
            ok,
            "{}:{}: not a minisign public key (expected the 56-character base64 line of minisign.pub)",
            path.display(),
            no + 1
        );
    }
}
