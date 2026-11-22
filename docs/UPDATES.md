# Updates

HostingOS can look for new releases, tell the owner about them and,
on a bare-binary install, install one in place after an admin approves it.
Nothing is installed automatically.

- Page: **Admin → Server → Updates** (`/admin/server/updates`), for the
  owner only (`system.manage`). In the optional multi-company mode the same
  page is the operator console's `/platform/updates`.
- CLI: `hostingos update check | status | apply <version> | rollback`,
  `hostingos version [--json]`.
- Code: `crates/hos-update` (verification, download, install state machine,
  watchdog) and `crates/hos-web/src/routes/platform/updates/` (update page,
  scheduling, maintenance mode, start-up handshake).
- Releasing and signing: [RELEASING.md](RELEASING.md).

## 1. Turning it on

Update checks are **off until a release source is configured**. There is no
built-in default repository. Without one, HostingOS never contacts a server
for updates and the update page says so.

| Variable | Meaning |
|---|---|
| `HOS_UPDATE_REPO` | GitHub repository `owner/name` whose Releases carry the signed manifest. |
| `HOS_UPDATE_MANIFEST_URL` | Alternative to the repository: the HTTPS URL of a `hostingos-manifest.json` (its signature at the same URL + `.minisig`). If both are set, this one wins. |
| `HOS_UPDATE_ALLOWED_HOSTS` | Extra hosts (comma separated) that downloads may come from or redirect to. The defaults are `api.github.com`, `github.com`, `objects.githubusercontent.com`, `release-assets.githubusercontent.com`, plus the host of `HOS_UPDATE_MANIFEST_URL`. |
| `HOS_UPDATE_HEALTH_TIMEOUT` | Seconds a freshly installed release has to report healthy (30–3600, default 300). |
| `HOS_UPDATE_BACKUP_COMMAND` | Shell command run before an install. `HOS_BACKUP_LABEL` holds the target version. If unset, `pg_dump` is used when it is on `PATH`. Otherwise the admin has to confirm that a backup exists. |
| `HOS_INSTALL_TYPE` | Overrides install detection: `binary`, `container` or `manual` (instructions only). The official image sets `container`. |
| `HOS_UPDATE_ROOT` | Root of the release layout (default: two levels above the running binary). |
| `HOS_SUPERVISED` | `1` when a supervisor other than systemd restarts the process on exit code 75. systemd is detected through `INVOCATION_ID`. |

The **channel** (`stable`, or `beta`, which also offers pre-releases),
**automatic checks**, an **e-mail address for notifications** and the
**deploy webhook** are set on the update page. They apply to every node.

### Privacy

A check is a plain HTTPS `GET` to the configured source with the User-Agent
`HostingOS-Updater/<version>`. Nothing else is sent: no telemetry, no
identifiers and no counters. The anonymous endpoints `/healthz` and
`/api/v1/ping` do not reveal the version. It is shown only to the owner.

## 2. Discovery

- The first check runs 30–150 seconds after start. After that, a check runs
  once a day at a random time. The check is claimed in the database, so in a
  cluster only one node checks per day. **Check now** on the update page (rate
  limited) and `hostingos update check` run a check on demand.
- GitHub source: `GET /repos/<owner>/<name>/releases?per_page=30`. Drafts are
  skipped, as are pre-releases on `stable`, versions not newer than the running
  one and releases without both `hostingos-manifest.json` and
  `hostingos-manifest.json.minisig`. The five newest candidates are verified.
- Every newer release whose signature verifies is recorded with its version,
  severity (`regular` or `security`), `min_upgradable_from`, `migrations` and
  `rollback_safe` flags, container image and Markdown notes. The notes are
  rendered with the same sanitising renderer as the rest of the app. Rejected
  manifests appear under "problems" and are never offered.
- When a newer version appears for the first time, the owner gets an
  in-app notice: a banner on the server pages and a discreet indicator in
  the admin panel's top bar. The
  notification address gets one e-mail.

## 3. Verification

1. **Signature.** The manifest bytes must carry a valid
   [minisign](https://jedisct1.github.io/minisign/) Ed25519 signature
   (pre-hashed `ED` format) from a trusted key. Verification uses the
   maintained `minisign-verify` crate.
2. **Trusted keys.** The public keys compiled into the binary are listed in
   `crates/hos-update/keys/trusted.pub`. **This file contains no key yet.** Until
   a build with the project key is installed, checks still run and
   new versions are announced, marked *unverified*, but **installs are
   refused** with an explanation.
3. **Key rotation.** A verified manifest may announce `next_keys`. They are
   stored as trusted, endorsed by the key that signed it. A later release can
   then be signed with the new key. `revoked_keys` removes endorsed keys.
   Compiled-in keys can only be removed by a new build.
4. **Rules for an offer.** The version must be newer than the running one, so
   no downgrades. `published_at` may be at most 24 h in the future and must not
   be older than the one already seen for that version (replay). The channel
   must match. The running version must be at least `min_upgradable_from`.
   There must be an artefact for this platform: a glibc build also accepts the
   static musl build of its architecture. The tag must equal the manifest
   version.
5. **Artefact.** The download must be HTTPS. Every redirect hop has to stay on
   the allow-list and pass the outbound policy (`hos_core::net`), with at most
   5 redirects. Connect, request and download timeouts are 10 s, 30 s and
   10 min. Size limits: manifest 256 KiB, notes 64 KiB, artefact 512 MiB. The
   size and SHA-256 come from the signed manifest and are checked while
   streaming. The tarball must also carry its own minisign signature. Before
   anything is installed, the database row is re-verified against the stored
   signed bytes.

## 4. Installing, by install type

The type is detected at start-up and shown on the update page.

### Bare binary (systemd)

The layout created by [deploy/install.sh](deploy/install.sh):

```text
/opt/hostingos/
  releases/<version>/{hostingos, static/, locales/}
  current  -> releases/<version>        (systemd runs current/hostingos)
  previous -> releases/<older version>
  state/{update-marker.json, update.lock, downloads/, backups/}
```

The service must be restarted by its supervisor when it exits with code
**75** (see [deploy/hostingos.service](deploy/hostingos.service):
`Restart=always`, `RestartForceExitStatus=75`).

To install, the admin presses **Install** and enters their password, plus the
TOTP code when 2FA is on. The CSRF token is required and the action is
written to the audit log. The install then runs these steps; progress is
shown live on `/admin/server/updates/runs/<id>` and survives the restart:

1. **Pre-flight.** Trusted keys exist and there is an artefact for this
   platform. A PostgreSQL advisory lock is taken (one update per cluster),
   plus a host lock file. No unfinished update may exist. The release
   directory must be writable, free disk space must be at least 3× the
   artefact + 200 MiB, and the database must be reachable.
2. **Maintenance mode.** It is turned on for the whole cluster. Visitors and
   clients get `503` with `Retry-After`. The server pages
   (`/admin/server/**`), the staff login, `/healthz`, `/readyz` and assets
   stay up. Job workers on every node stop claiming jobs,
   and running jobs get up to 2 minutes to finish.
3. **Download, verification and unpack.** The archive is unpacked into a
   staging directory: regular files only, a single top directory, no
   absolute paths, `..` or links. Then `hostingos version --json` of the new
   binary must report the expected version and target.
4. **Backup.** `HOS_UPDATE_BACKUP_COMMAND`, otherwise `pg_dump --format=custom`
   into `state/backups/` (mode 0600), otherwise the admin's "I have a backup"
   confirmation.
5. **Switch.** A watchdog marker is written. Then `previous` becomes the old
   release and `current` the new one; each link is swapped atomically.
6. **Migrations.** The new binary runs `hostingos migrate`. If that fails, the
   old release is put back, maintenance mode is turned off and the run is
   recorded as failed.
7. **Restart.** The marker gets a deadline (`HOS_UPDATE_HEALTH_TIMEOUT`). The
   process exits with 75 and systemd starts `current/hostingos`.
8. **Confirmation.** The new process finds the marker and checks the database,
   that every embedded migration is applied, and its own `/readyz`. If all is
   well, the run succeeds, maintenance mode ends and old releases are pruned
   (the current and previous ones are kept).
9. **Automatic rollback.** If the new release is not healthy by the deadline,
   or does not get that far within 3 starts, `current` is pointed back at the
   old release, the marker records it and the process exits with 75. The old
   binary then starts, records "rolled back", ends maintenance mode and
   notifies the admins.

An interrupted install, for example a power loss during the switch, is
handled by the marker on the next start. Swapped-but-unconfirmed releases go
through the same trial-and-rollback path. A stale host lock (older than 3 h)
is ignored. Maintenance mode left on without an active run is turned off at
start-up, and an admin can also end it on the update page.

From the command line: `hostingos update apply <version>` (it asks for
confirmation; use `--yes` and `--backup-confirmed` for scripts). The running
service notices that `current` changed and restarts into it.
`hostingos update rollback` points `current` back to `previous`.

### Docker, Compose, Kubernetes, Coolify

A container never replaces its own binary: the image is immutable. The
update page shows the new image tag and the commands:

```sh
docker compose pull && docker compose up -d   # the one-shot `migrate` service runs first
```

The `edge` and `sha-…` images built from `main`
([RELEASING.md](RELEASING.md) §8) are not releases: they carry no signed
manifest, so the update page never announces them. An installation on
`edge` updates with the same two commands, whenever its operator decides.

Where the platform starts the new container first and runs commands only
afterwards (Coolify's post-deployment command, for example), set
`HOS_MIGRATE_ON_START=1` (or use `hostingos serve --migrate`): the container
then applies pending migrations before it opens its port, and a separate
`migrate` step is unnecessary (docs/DEPLOYMENT.md, "Migrations").

Optionally, a **deploy webhook** can be configured, for example a
Coolify/Portainer deploy URL or a CI `workflow_dispatch` proxy. It must use
HTTPS and must not contain credentials in the URL. Its secret is stored
encrypted. **Trigger deployment** requires re-authentication, works only for
signature-verified releases, and sends:

- `GET`, or `POST` with the JSON `{"event":"hostingos.update","version",
  "image","channel","timestamp"}`;
- `Authorization: Bearer <secret>`;
- for `POST`, also `X-HostingOS-Signature: sha256=<HMAC-SHA256 of the body
  with the secret>`.

Redirects are refused. The answer is recorded in the history. The new
container's start-up then marks the run as completed.

### Unknown

If neither layout is recognised (for example a binary started by hand),
the update page only shows the release notes and points to these instructions.

## 5. Migration and rollback policy

- **Expand/contract.** A release only *adds* to the schema: new tables, new
  nullable columns, new indexes. Code of the previous release keeps working
  against the new schema. Destructive steps (dropping or renaming columns,
  tightening constraints) come in a later release, once no supported version
  still uses the old shape.
- **`rollback_safe`** in the manifest says the previous release runs correctly
  on the migrated schema. If it is absent, it counts as `false`.
- **Rolling back the binary never rolls back the database.** Migrations are
  forward-only. After a rollback of a release that was not `rollback_safe`
  and whose migrations ran, **restore the backup taken before the update**.
  The update page and the run log say so explicitly.
- The old binary can run against a newer schema: `serve` does not touch a
  database that is ahead of it (not even with `--migrate` /
  `HOS_MIGRATE_ON_START`); it logs this once and serves, and `/readyz`
  reports `"schema":"ahead"`. Its `hostingos migrate` refuses, since it does
  not know the newer migrations. That refusal is intended.
- `min_upgradable_from` makes admins pass through a release that carries a
  required data migration.

## 6. Multi-node notes

- Maintenance mode, the job pause and the update lock are cluster-wide
  (stored in the database). The binary swap happens on the node where the admin
  pressed Install. Update the other nodes with `hostingos update apply
  <version>` (or your configuration management) while maintenance mode is
  still on, or after the first node confirmed.
- A process whose `current` link was switched by another process on the same
  host restarts itself into the new release.

## 7. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| "Update checks are off" | Set `HOS_UPDATE_REPO` or `HOS_UPDATE_MANIFEST_URL`, then restart. |
| Releases shown as *unverified*, installs refused | This build trusts no signing key. Install a build that contains the project's public key. |
| "no supervisor detected" | Run under systemd with the unit file from `docs/deploy/`, or set `HOS_SUPERVISED=1` when another supervisor restarts the process on exit code 75. |
| Stuck in maintenance mode | End it on the update page (re-authentication required), or restart the service: a start-up without an active run turns it off. |
| Rolled back | Read the run log at `/admin/server/updates/runs/<id>` and the service journal (`journalctl -u hostingos`). Restore the backup if the release was not `rollback_safe`. |
