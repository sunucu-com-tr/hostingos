#!/usr/bin/env python3
"""Builds the release notes and the update manifest of a HostingOS release.

    release-manifest.py --version 0.2.0 --repo owner/name \
        --image ghcr.io/owner/hostingos --dist dist --notes-out notes.md

Reads:
  * CHANGELOG.md — the `## [<version>] - <date>` section becomes the release
    notes. `### Security` in it makes the release a security release. An
    optional comment line in the section sets the update metadata:
        <!-- release: rollback_safe=true min_upgradable_from=0.2.0 -->
  * dist/hostingos-<version>-<target>.tar.gz — one artefact per target
    (size + SHA-256 go into the manifest);
  * git — whether migrations were added since the previous `v*` tag;
  * crates/hos-update/keys/next.pub (optional) — public keys announced for
    rotation (`next_keys`), same format as trusted.pub;
  * crates/hos-update/keys/revoked.txt (optional) — key ids (16 hex digits)
    to revoke (`revoked_keys`).

Writes dist/hostingos-manifest.json (schema 1, see crates/hos-update/src/
manifest.rs and docs/RELEASING.md) and the notes file. Standard library
only; fails loudly on anything unexpected.
"""

import argparse
import datetime
import hashlib
import json
import pathlib
import re
import subprocess
import sys

SEMVER = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(-[0-9A-Za-z.-]+)?$")
TARGETS = ("x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl")
MAX_NOTES = 64 * 1024
MAX_MANIFEST = 256 * 1024
KEY = re.compile(r"^RW[A-Za-z0-9+/]{54}$")
KEY_ID = re.compile(r"^[0-9A-F]{16}$")
MIGRATIONS = "crates/hos-db/migrations"


def fail(msg: str) -> None:
    print(f"::error::{msg}", file=sys.stderr)
    sys.exit(1)


def changelog_section(version: str) -> str:
    text = pathlib.Path("CHANGELOG.md").read_text(encoding="utf-8")
    head = re.compile(r"^## \[" + re.escape(version) + r"\](\s+-\s+.*)?$", re.M)
    m = head.search(text)
    if not m:
        fail(f"CHANGELOG.md has no '## [{version}] - <date>' section")
    rest = text[m.end():]
    nxt = re.search(r"^## \[", rest, re.M)
    body = rest[: nxt.start()] if nxt else rest
    # Link reference definitions at the end of the file are not notes.
    body = re.sub(r"^\[[^\]]+\]:\s+\S+\s*$", "", body, flags=re.M)
    return body.strip()


def metadata(section: str) -> dict:
    meta = {"rollback_safe": False, "min_upgradable_from": None}
    for m in re.finditer(r"<!--\s*release:(.*?)-->", section, re.S):
        for pair in m.group(1).split():
            key, _, value = pair.partition("=")
            if key == "rollback_safe":
                if value not in ("true", "false"):
                    fail(f"rollback_safe must be true or false, not {value!r}")
                meta["rollback_safe"] = value == "true"
            elif key == "min_upgradable_from":
                if not SEMVER.match(value):
                    fail(f"min_upgradable_from {value!r} is not a version")
                meta["min_upgradable_from"] = value
            else:
                fail(f"unknown release metadata {key!r}")
    return meta


def git(*args: str) -> str:
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout.strip()


def migrations_added(tag: str) -> bool:
    try:
        prev = git("describe", "--tags", "--abbrev=0", "--match", "v*", f"{tag}^")
    except subprocess.CalledProcessError:
        # First release: it creates the schema.
        return True
    return bool(git("diff", "--name-only", "--diff-filter=A", f"{prev}..{tag}", "--", MIGRATIONS))


def key_lines(path: pathlib.Path, pattern: re.Pattern, what: str) -> list:
    if not path.exists():
        return []
    out = []
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        token = line.split()[0]
        if not pattern.match(token):
            fail(f"{path}: malformed {what}: {token!r}")
        out.append(token)
    return out


def sha256(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--image", required=True)
    ap.add_argument("--dist", required=True)
    ap.add_argument("--notes-out", required=True)
    ap.add_argument("--published-at", help="RFC 3339 (default: now)")
    a = ap.parse_args()

    if not SEMVER.match(a.version):
        fail(f"{a.version!r} is not a semantic version")
    if not re.match(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$", a.repo):
        fail(f"{a.repo!r} is not owner/name")
    tag = f"v{a.version}"
    dist = pathlib.Path(a.dist)

    section = changelog_section(a.version)
    meta = metadata(section)
    notes = re.sub(r"<!--\s*release:.*?-->\s*", "", section, flags=re.S).strip()
    if len(notes.encode()) > MAX_NOTES:
        fail("release notes exceed 64 KiB")
    pathlib.Path(a.notes_out).write_text(notes + "\n", encoding="utf-8")

    artifacts = []
    for target in TARGETS:
        name = f"hostingos-{a.version}-{target}.tar.gz"
        path = dist / name
        if not path.is_file():
            fail(f"missing artefact {path}")
        artifacts.append(
            {
                "target": target,
                "name": name,
                "url": f"https://github.com/{a.repo}/releases/download/{tag}/{name}",
                "size": path.stat().st_size,
                "sha256": sha256(path),
            }
        )

    published = a.published_at or datetime.datetime.now(datetime.timezone.utc).replace(
        microsecond=0
    ).isoformat().replace("+00:00", "Z")
    manifest = {
        "schema": 1,
        "product": "hostingos",
        "version": a.version,
        "channel": "beta" if "-" in a.version else "stable",
        "published_at": published,
        "severity": "security" if re.search(r"^### Security\s*$", section, re.M) else "regular",
        "migrations": migrations_added(tag),
        "rollback_safe": meta["rollback_safe"],
        "notes_md": notes,
        "container_image": f"{a.image}:{a.version}",
        "artifacts": artifacts,
        "next_keys": key_lines(pathlib.Path("crates/hos-update/keys/next.pub"), KEY, "public key"),
        "revoked_keys": key_lines(pathlib.Path("crates/hos-update/keys/revoked.txt"), KEY_ID, "key id"),
    }
    if meta["min_upgradable_from"]:
        manifest["min_upgradable_from"] = meta["min_upgradable_from"]
    raw = json.dumps(manifest, indent=2, ensure_ascii=False).encode() + b"\n"
    if len(raw) > MAX_MANIFEST:
        fail("manifest exceeds 256 KiB")
    (dist / "hostingos-manifest.json").write_bytes(raw)
    print(json.dumps({k: v for k, v in manifest.items() if k != "notes_md"}, indent=2))


if __name__ == "__main__":
    main()
