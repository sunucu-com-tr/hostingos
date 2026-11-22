# Releasing

How a HostingOS release is built, signed and published, and what the
project owner has to set up once. The updater side is described in
[UPDATES.md](UPDATES.md).

## 1. What a release contains

Pushing a tag `vX.Y.Z` (or `vX.Y.Z-beta.N`) runs
[`.github/workflows/release.yml`](../.github/workflows/release.yml). It
produces these assets on a GitHub Release:

| Asset | Content |
|---|---|
| `hostingos-X.Y.Z-x86_64-unknown-linux-musl.tar.gz` | static binary, `static/`, `locales/`, `README.md`, `CHANGELOG.md`, `SECURITY.md`, licence files (once there are any), `deploy/hostingos.service`, `deploy/install.sh` |
| `hostingos-X.Y.Z-aarch64-unknown-linux-musl.tar.gz` | the same for 64-bit ARM |
| `*.cdx.json` | CycloneDX SBOM per target |
| `hostingos-manifest.json` | the signed update manifest (schema 1, see below) |
| `SHA256SUMS` | checksums of the files above |
| `*.minisig` | a minisign signature for every file above |

It also produces:

- `ghcr.io/<owner>/hostingos:X.Y.Z` (plus `X.Y` and `latest` for stable
  releases): a multi-arch image (amd64 and arm64) built from the same
  binaries, with SBOM and provenance;
- GitHub build-provenance attestations for the tarballs, the manifest and
  the image.

The build:

- links statically against musl, so one binary runs on any Linux;
- pins the toolchain (`RUSTUP_TOOLCHAIN`) and uses `--locked`;
- uses the commit time for `SOURCE_DATE_EPOCH`, which is also the build date
  shown in the console;
- remaps source paths;
- packs tarballs with sorted entries, fixed owner and mtime, and `gzip -n`.

The release is created **last**, so updaters never see a manifest whose
image does not exist yet.

## 2. One-time setup (owner)

These steps need the owner's accounts. Nothing here can be done from the
code.

1. **Create the GitHub repository and push the code.** A public
   repository gets free `ubuntu-24.04-arm` runners for the aarch64 build.
   For a private one, check that arm64 runners are available on your plan.
   Otherwise, build aarch64 on `ubuntu-24.04` with a cross toolchain.
2. **Generate the signing key**, offline, on a trusted machine, with
   [minisign](https://jedisct1.github.io/minisign/) 0.11 or later:

   ```sh
   minisign -G -p hostingos-release.pub -s hostingos-release.key
   # choose a strong password; minisign prints the key id
   ```

   Keep `hostingos-release.key` and its password **out of the repository**:
   store them in a password manager and keep an offline backup. Anyone with
   both can publish updates that every installation accepts.
3. **Trust the public key.** Copy the second line of `hostingos-release.pub`
   (56 characters, starting with `RW`) into
   `crates/hos-update/keys/trusted.pub`, optionally followed by a label, and
   commit the change. Every binary built from then on verifies releases with
   it. Binaries built before have no key: they keep announcing updates but
   refuse to install them.
4. **Add the secrets** to a GitHub environment called `release`
   (Settings → Environments → New environment). Add required reviewers so a
   tag alone cannot publish. Then:

   ```sh
   gh secret set MINISIGN_SECRET_KEY --env release < hostingos-release.key
   gh secret set MINISIGN_PASSWORD --env release   # paste the password
   ```

5. **Protect the tags.** Add a ruleset that lets only maintainers create
   `v*` tags, and require reviews on `main`.
6. **Optionally publish the image.** After the first release, make the
   `hostingos` package public under the repository's Packages, so
   installations can pull it without a login.
7. **Point installations at the repository.** Set `HOS_UPDATE_REPO=<owner>/<repo>`
   in their environment (docs/UPDATES.md), then restart.

## 3. Cutting a release

1. Make sure CI on `main` is green.
2. Edit `CHANGELOG.md`: move the `Unreleased` entries under
   `## [X.Y.Z] - YYYY-MM-DD`. That section becomes the release notes and the
   manifest's `notes_md` (at most 64 KiB). The section also controls the
   update metadata:
   - `### Security` marks a security release. The console banner and the
     e-mail say so.
   - An optional comment line sets the remaining fields:

     ```text
     <!-- release: rollback_safe=true min_upgradable_from=0.3.0 -->
     ```

     - `rollback_safe=true` only when the previous release works correctly
       on the schema after this release's migrations (expand-only changes,
       see UPDATES.md §5). The default is `false`.
     - `min_upgradable_from` is the oldest version that may install this one
       directly.
   - `migrations` is computed by the workflow: it is set when files were
     added under `crates/hos-db/migrations/` since the previous `v*` tag.
3. Set `version` in `[workspace.package]` of the root `Cargo.toml` to
   `X.Y.Z` and run `cargo check` so `Cargo.lock` follows. The workflow
   refuses a tag that does not match.
4. Commit, then tag and push:

   ```sh
   git tag -s vX.Y.Z -m "HostingOS X.Y.Z"
   git push origin vX.Y.Z
   ```

5. Approve the `release` environment when the workflow asks. Afterwards,
   check the release page and the image.

Pre-releases (`X.Y.Z-beta.N`) are marked as pre-release on GitHub, are not
tagged `latest`, and are offered only to installations on the `beta`
channel.

## 4. Verifying a release by hand

```sh
minisign -Vm hostingos-X.Y.Z-x86_64-unknown-linux-musl.tar.gz -p hostingos-release.pub
minisign -Vm SHA256SUMS -p hostingos-release.pub && sha256sum -c SHA256SUMS
gh attestation verify hostingos-X.Y.Z-x86_64-unknown-linux-musl.tar.gz --repo <owner>/<repo>
gh attestation verify oci://ghcr.io/<owner>/hostingos:X.Y.Z --repo <owner>/<repo>
```

To reproduce a binary, check out the tag, use the same toolchain and set
`SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)`,
`HOS_BUILD_COMMIT=$(git rev-parse HEAD)` and the `--remap-path-prefix` flags
of the workflow. Then build with `--locked --target <triple>` on Linux with
musl-tools.

## 5. Manifest format (schema 1)

```json
{
  "schema": 1,
  "product": "hostingos",
  "version": "0.2.0",
  "channel": "stable",
  "published_at": "2026-12-01T10:00:00Z",
  "severity": "regular",
  "min_upgradable_from": "0.1.0",
  "migrations": true,
  "rollback_safe": false,
  "notes_md": "### Added\n- …",
  "container_image": "ghcr.io/<owner>/hostingos:0.2.0",
  "artifacts": [
    {
      "target": "x86_64-unknown-linux-musl",
      "name": "hostingos-0.2.0-x86_64-unknown-linux-musl.tar.gz",
      "url": "https://github.com/<owner>/<repo>/releases/download/v0.2.0/hostingos-0.2.0-x86_64-unknown-linux-musl.tar.gz",
      "size": 12345678,
      "sha256": "…64 hex…"
    }
  ],
  "next_keys": [],
  "revoked_keys": []
}
```

- The signature (`hostingos-manifest.json.minisig`) covers the exact bytes.
  Each artefact has its own `<url>.minisig`.
- `channel` is `beta` for versions with a pre-release part, otherwise
  `stable`. A pre-release on `stable` is rejected.
- Limits: manifest 256 KiB, notes 64 KiB, artefact 512 MiB.
- The code that defines the format is `crates/hos-update/src/manifest.rs`.
  [`.github/scripts/release-manifest.py`](../.github/scripts/release-manifest.py)
  writes it, and
  [`.github/scripts/sign-release.sh`](../.github/scripts/sign-release.sh)
  signs it. The signing script refuses keys that `trusted.pub` and
  `next.pub` do not contain.

## 6. Key rotation and revocation

Planned rotation, from key A to key B:

1. Generate key B as in §2.
2. Put B's public key into `crates/hos-update/keys/next.pub` **and**
   `trusted.pub`. Fresh builds then trust both keys.
3. Release N, still signed with A, announces B in `next_keys`.
   Installations that verify N store B as trusted, endorsed by A.
4. Once enough installations run N or later, replace the `release`
   environment secrets with key B. Empty `next.pub`, and remove A from
   `trusted.pub` in a later release.
5. To stop installations from accepting A, add A's key id (16 hex digits,
   printed by `minisign -G` and in the comment line of the `.pub` file) to
   `crates/hos-update/keys/revoked.txt`. The next manifest, signed with B,
   carries it in `revoked_keys`. Updaters drop endorsed keys listed there. A
   compiled-in key is removed only by installing a build without it.

If a key is compromised:

1. Remove the secret.
2. Generate a new key.
3. Publish a release signed with a key the installations still trust,
   announcing the new key and revoking the compromised one.
4. Tell operators, through `SECURITY.md` channels, to verify the next
   release by hand.

Installations that only trust the compromised key need a manual
reinstall (`docs/deploy/install.sh`).

## 7. Not verified yet

The workflows were written without running on GitHub. The first tagged
release should be watched for:

- the musl build of `aws-lc-sys` and `ring` with `musl-gcc` and the kernel
  header include path set up in the workflow;
- the availability of the `ubuntu-24.04-arm` runner;
- the `cargo-cyclonedx` output path (`crates/hos-web/*.cdx.json`);
- minisign on the runner producing pre-hashed signatures (the script checks
  this and fails otherwise).
