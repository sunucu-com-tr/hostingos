# Deploying HostingOS

HostingOS runs one hosting company: its storefront, client area and admin
panel, all on the host of `HOS_BASE_URL`. One binary (`hostingos`) contains
the web server, the job worker, the cron scheduler, the maintenance loop and
the admin CLI. Templates and SQL
migrations are compiled in; `static/` and `locales/` are read from disk
(`HOS_STATIC_DIR`, `HOS_LOCALES_DIR`). The only external dependency is
PostgreSQL 16 (Neon or self-hosted).

```
hostingos serve               # web + embedded worker/scheduler (default command)
hostingos serve --migrate     # apply pending migrations first (= HOS_MIGRATE_ON_START=1)
hostingos serve --no-worker   # web only
hostingos worker              # worker + scheduler + maintenance only
hostingos migrate             # apply pending migrations
hostingos setup --email owner@example.com [--company "Acme Hosting"]
                              # first installation: migrations + company + owner
hostingos create-staff --email support@example.com --role support
hostingos healthcheck [--path /readyz]   # container health check (exit 0 / 1)
hostingos jobs run-once [--max-jobs 100]
hostingos seed-demo [--reset-passwords]
hostingos version [--json]
hostingos update check|status|apply <version>|rollback   # docs/UPDATES.md
```

Passwords for `setup` and `create-staff` come from `--password` or
`HOS_NEW_PASSWORD`; without one a random password is printed once. `SIGTERM` /
`Ctrl-C` stops accepting requests, lets running jobs finish (up to 45 s) and
exits. Exit code **75** means "restart me into the active release" (after an
update or a rollback); the supervisor must restart on it (systemd:
`docs/deploy/hostingos.service`).

Install options:

* **Docker Compose** (below) or any container platform (Coolify, Kubernetes):
  the published image `ghcr.io/sunucu-com-tr/hostingos` — `edge` (the
  newest build of `main`, not a signed release) or `sha-<commit>` (one
  fixed build) until signed `X.Y.Z` releases exist — or a local build.
* **Bare binary under systemd**: `docs/deploy/install.sh` verifies the signed
  release archive and creates `/opt/hostingos/{releases/<version>,current,state}`;
  `docs/deploy/hostingos.service` is the hardened unit. Only this layout can
  update itself in place (admin panel → Server → Updates).

Updates (checking, notifications, one-click install with backup and automatic
rollback, deploy webhook for containers): [UPDATES.md](UPDATES.md). Building
and signing releases: [RELEASING.md](RELEASING.md).

## Environment

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `DATABASE_URL` | yes | – | Pooled connection string (Neon: the `-pooler` host) |
| `DATABASE_URL_DIRECT` | recommended | `DATABASE_URL` | Direct connection: migrations and the worker's `LISTEN/NOTIFY` |
| `HOS_SECRET_KEY` | yes | – | 32 random bytes, base64 (`openssl rand -base64 32`). Encrypts integration credentials and TOTP secrets — **back it up** |
| `HOS_BASE_URL` | prod | `http://localhost:<port>` | The company's public URL (links in e-mails, certificates); `https` turns on Secure cookies |
| `HOS_MODE` | – | `single` | `single`: one hosting company, served on every host that reaches the app. `multi`: the advanced multi-company mode ([below](#advanced-multi-company-mode)). Another value stops `hostingos` at start-up |
| `HOS_BIND` | – | `127.0.0.1:8080` | Listen address (`0.0.0.0:8080` in containers) |
| `HOS_TRUST_PROXY` | behind a proxy | `false` | Trust `X-Forwarded-For/-Proto` (client IP for rate limits, audit, https detection) — from the peers in `HOS_TRUSTED_PROXIES` ([Reverse proxies](#reverse-proxies-and-the-client-address)) |
| `HOS_TRUSTED_PROXIES` | recommended behind a proxy | – | Comma-separated addresses / CIDR ranges (IPv4, IPv6) of the reverse proxies that connect to the app. With `HOS_TRUST_PROXY` the forwarding headers (`X-Forwarded-For`, `X-Forwarded-Proto`, `HOS_CLIENT_IP_HEADER`) count only on connections from these addresses; any other peer is taken at its own address. Unset: believed from every peer, as before (warning at start-up). A malformed entry stops `hostingos` at start-up |
| `HOS_TRUSTED_PROXY_HOPS` | behind a proxy | `1` | Proxies that append to `X-Forwarded-For` (with `HOS_TRUST_PROXY`): the client IP is the entry this many places from the right; `0` ignores the header. Caddy and Traefik ignore the `X-Forwarded-For` they receive unless told to trust the sender, so a CDN in front of them adds a hop only when Caddy's `trusted_proxies` / Traefik's `forwardedHeaders.trustedIPs` list the CDN's ranges — then `2`. Otherwise use `HOS_CLIENT_IP_HEADER` |
| `HOS_CLIENT_IP_HEADER` | behind a CDN | – | With `HOS_TRUST_PROXY`: a header in which the outermost proxy puts the visitor's address, used before `X-Forwarded-For` when it holds exactly one valid address. Cloudflare: `CF-Connecting-IP`. Only safe when the server cannot be reached around the CDN (firewall to the CDN's ranges) |
| `HOS_TLS_ASK_SECRET` | with Caddy | – | Shared secret for `GET /internal/tls-allow` from a non-loopback proxy |
| `HOS_WORKER_CONCURRENCY` | – | `8` | Parallel jobs per worker process (1–256) |
| `HOS_JOB_QUEUE_NAMESPACE` | – | – | Only for a second installation (staging, QA) sharing one database with another: its processes enqueue and claim jobs in `<namespace>.<queue>`, so neither installation runs the other's jobs. Every process of that installation must set the same value (`a–z`, `0–9`, `_`, `-`) |
| `HOS_MAX_BODY_BYTES` | – | `2097152` | Request body limit |
| `HOS_HTTP_HEADER_TIMEOUT_SECS` | – | `120` | Seconds a client has to send a request's headers; also how long an idle keep-alive connection stays open. `0` = no limit ([HTTP timeouts](#http-timeouts)) |
| `HOS_HTTP_BODY_TIMEOUT_SECS` | – | `60` | Seconds a client has to send the whole request body, else `408`. Raise it with `HOS_MAX_BODY_BYTES` for large uploads over slow links. `0` = no limit |
| `HOS_HTTP_REQUEST_TIMEOUT_SECS` | – | `120` | Seconds the server may work on a received request before it answers `503` and stops. `0` = no limit |
| `HOS_MIGRATE_ON_START` | – | off | `1`: `hostingos serve` applies pending migrations before it serves (as `serve --migrate`; always on in demo mode). See [Migrations](#migrations) |
| `HOS_OUTBOUND_POLICY` | – | `private` | Which addresses admin-configured integration endpoints may reach (see [Outbound connections](#outbound-connections-ssrf-guard)). An invalid value stops `hostingos` at start-up |
| `HOS_ALLOW_MOCK_INTEGRATIONS` | demo only | off | `1` enables the `mock` payment gateway, hypervisor, panel and registrar (a mock gateway lets anyone pay any invoice). Implied by debug mode; a warning is logged at start-up. **Never in production** |
| `HOS_DEBUG_MAIL` | dev | off | `1` lets the log mailer (no mail transport configured) write message bodies — reset / confirmation links included — to the log. **Never in production** |
| `HOS_DEBUG` | – | debug builds | Development mode: detailed error pages, mock integrations allowed — never in production (release builds default to off) |
| `HOS_STATIC_DIR` / `HOS_LOCALES_DIR` | – | `./static`, `./locales` | Asset and translation directories |
| `HOS_LOG_FORMAT` | – | text | `json` for structured logs |
| `RUST_LOG` | – | `info` | Log filter, e.g. `info,sqlx=warn` |
| `ANTHROPIC_API_KEY` / `HOS_AI_MODEL` | – | – / `claude-opus-5-5` | AI assistant features (off without a key) |
| `GOOGLE_CLIENT_ID/SECRET`, `GITHUB_CLIENT_ID/SECRET` | – | – | Default OAuth login apps (the admin panel can set its own) |
| `HOS_NEW_PASSWORD` | CLI only | – | Password for `setup` and `create-staff` |
| `HOS_UPDATE_REPO` / `HOS_UPDATE_MANIFEST_URL` | – | off | Release source for update checks (GitHub `owner/name`, or an HTTPS manifest URL). **No default: checks are off until set** ([UPDATES.md](UPDATES.md)) |
| `HOS_UPDATE_ALLOWED_HOSTS` | – | GitHub hosts | Extra download hosts for updates (comma separated) |
| `HOS_UPDATE_HEALTH_TIMEOUT` | – | `300` | Seconds a newly installed release has to become healthy before it is rolled back (30–3600) |
| `HOS_UPDATE_BACKUP_COMMAND` | binary installs | `pg_dump` if present | Command run before an in-place update (`HOS_BACKUP_LABEL` = target version) |
| `HOS_INSTALL_TYPE` / `HOS_UPDATE_ROOT` / `HOS_SUPERVISED` | – | detected | Override install detection, the release-layout root, supervisor detection ([UPDATES.md](UPDATES.md)) |

Never commit a filled-in env file; `docs/deploy/env.example` is the template.

## Docker Compose (single host)

`docker-compose.yml` runs PostgreSQL 16, a one-shot `migrate`, the app
(web + embedded worker) and Caddy 2, which obtains the certificate for
`HOS_BASE_URL`'s host on demand (`docs/deploy/Caddyfile`).

```sh
cp docs/deploy/env.example .env.production       # fill in the secrets
docker compose --env-file .env.production up -d
docker compose --env-file .env.production run --rm app setup --email owner@example.com
```

`setup` creates the company and its owner account once and prints the
password; sign in at `<HOS_BASE_URL>/auth/login`.

**Image.** The compose file never builds: it runs `HOS_IMAGE:HOS_TAG`,
default `ghcr.io/sunucu-com-tr/hostingos:edge`, and pulls it when it is
missing on the host (a failed pull stops the start; nothing is built in
its place). Tags:

| Tag | Published by | Content |
|---|---|---|
| `edge` | `.github/workflows/ci.yml`, every push to `main` after lint, tests, the dependency audit and the end-to-end demo test have passed | the newest build of `main`; moves; not a signed release |
| `sha-<7-digit commit>` | the same run | the same build; never moves |
| `X.Y.Z`, `X.Y`, `latest` | `.github/workflows/release.yml`, `v*` tags ([RELEASING.md](RELEASING.md)) | signed releases; none published yet |

CI builds the `edge` / `sha-…` image from the Dockerfile (`runtime` stage)
for linux/amd64 and linux/arm64, each on a native runner, with OCI labels
(source, revision, licence) and a GitHub build provenance attestation.
After the first image has been published, `docker buildx imagetools
inspect ghcr.io/sunucu-com-tr/hostingos:edge` lists its platforms and
`gh attestation verify oci://ghcr.io/sunucu-com-tr/hostingos:edge --repo
sunucu-com-tr/hostingos` checks the attestation.

`docker-compose.quickstart.yml` is a smaller variant for a local trial
(app and PostgreSQL on `http://localhost:8080`, no Caddy, migrations on
start; README "Quick start").

**Building locally.** `docker-compose.build.yml` builds the image from the
checkout as `hostingos:local` (`pull_policy: build`, never pulled); add it
to every command, e.g. with
`export COMPOSE_FILE=docker-compose.yml:docker-compose.build.yml`, then
`docker compose --env-file .env.production up -d --build`. The
repository's `.dockerignore` keeps `.env*`, `.git` and `target/` out of
the build context (the Dockerfile copies only `Cargo.*`, `crates/`,
`static/` and `locales/`, so nothing else reaches an image layer either
way). Set `HOS_BUILD_COMMIT=$(git rev-parse HEAD)` and
`SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)` in the shell (or pass them
as `--build-arg` to `docker build`) to show the commit and build date on
the server page.

The image runs as uid 10001, listens on 8080 and has
`/healthz` (liveness) and `/readyz` (database reachable and migrated). Its `HEALTHCHECK`
is `hostingos healthcheck`: the binary sends a plain HTTP GET to `/healthz`
on `HOS_BIND` and exits 0 or 1 (`--path /readyz` checks the database too) —
no curl, no shell, nothing written, so it works for the non-root user on a
read-only root filesystem. The compose file runs it with a read-only root
filesystem, no capabilities and `no-new-privileges`.

Updating (pin a build with `HOS_TAG=sha-…`, or stay on `edge`):

```sh
docker compose --env-file .env.production pull
docker compose --env-file .env.production up -d     # `migrate` runs first
```

The container never replaces itself; the admin panel's update page
(Server → Updates, `/admin/server/updates`) shows new versions and can call
a deploy webhook (Coolify, Portainer, CI) — see [UPDATES.md](UPDATES.md).

DNS: point `HOS_BASE_URL`'s host at the server. Caddy asks
`http://app:8080/internal/tls-allow?domain=…&secret=…` before it requests a
certificate; the app answers `200` only for its own host, so nobody can make
the server request certificates for arbitrary names.

## First start

1. `hostingos setup --email owner@example.com --company "…"` (compose:
   `docker compose … run --rm app setup --email …`). It applies the
   migrations and creates the company and its owner account; the password
   comes from `--password` / `HOS_NEW_PASSWORD` or is generated and printed
   once. Running it again only says that the company exists.
2. Start the server (`hostingos serve`). Every host that reaches it shows
   the company; there is no "site not found" page. A server started before
   `setup` (for example a container whose platform runs commands only once
   it is up) stays up, answers every page with `503` and says in its log to
   run `setup`; the company appears within seconds after `setup`, without a
   restart.
3. Sign in at `<HOS_BASE_URL>/auth/login` as the owner and turn on TOTP 2FA
   under your account. In the admin panel: company details, currencies,
   payment gateways, registrars, servers and products; staff under Staff
   (or `hostingos create-staff --email … --role support`).
4. The owner — and only the owner (`system.manage` permission) — has
   **Server** in the admin menu:
   * `/admin/server`: version, uptime, database, migrations and health;
   * `/admin/server/updates`: update checks, installing an update (asks for
     the owner's password again, and the 2FA code when 2FA is on),
     maintenance mode, the deploy webhook. Every action is in the audit log
     under the owner's name (`server.update.*`).

   The job queue and the audit log are under System (`/admin/system/jobs`,
   `/admin/system/audit`).

With more than one company in the database (for example after a
multi-company test) the server refuses to start in `single` mode and says
why.

## Migrations

Migrations are forward-only and idempotent. A new version must not serve
before its migrations are applied; three ways, all safe together:

* `hostingos migrate` before the new version starts (compose: the one-shot
  `migrate` service; in-place updates do it themselves);
* `hostingos serve --migrate`, or `HOS_MIGRATE_ON_START=1` with the image's
  default command: the server applies pending migrations before it opens
  its port. Use this on platforms that run commands only once the new
  container is up (Coolify's post-deployment command, for example) — there
  the separate `migrate` command is then unnecessary. Demo mode always does
  it. A failed migration stops the start with the error in the log (exit
  code 1); the new container never opens its port or turns healthy;
* every run — `migrate`, `serve --migrate`, `setup` — holds a PostgreSQL
  advisory lock, so replicas starting together and a `migrate` running at
  the same time take turns (the log says `another process is applying
  migrations; waiting`) instead of racing. They need `DATABASE_URL_DIRECT`
  (or a `DATABASE_URL` without a transaction-mode pooler).

A server that finds its database **behind** (a migration of this release
is not applied, for example because `migrate` runs later) logs `the
database schema is behind this release` and answers every page with the
maintenance page (`503`, `Retry-After`) and `/readyz` with `503`
(`"schema":"behind"`) instead of failing requests on missing tables. It
re-checks every few seconds (and on every `/readyz`) and serves as soon as
the migrations are in — no restart. A database **ahead** of the release (a
newer release migrated it; normal for an old container during a rolling
deploy, or after going back to a previous version) is logged once and
served; `/readyz` says `"schema":"ahead"`.

## Reverse proxies and the client address

Rate limits, sessions, login history and the audit log use the client's
address. Directly exposed, that is the TCP peer. Behind a reverse proxy set
`HOS_TRUST_PROXY=true` so it is read from the proxy's headers
(`HOS_TRUSTED_PROXY_HOPS`, `HOS_CLIENT_IP_HEADER`), and set
`HOS_TRUSTED_PROXIES` to the proxy's address, so only the proxy can set
them — anyone else who reaches the port is taken at their own address:

| Proxy | `HOS_TRUSTED_PROXIES` |
|---|---|
| On the same host (Caddy / nginx in front of `127.0.0.1:8080`) | `127.0.0.1,::1` |
| In Docker, on a network shared with the app (Compose, Coolify, Traefik / Caddy containers) | the proxy container's address on that network, or the network's range: `docker network inspect <network> --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}'` |
| Elsewhere (load balancer, Kubernetes ingress) | the addresses the proxy connects from |

The address that counts is the one the app sees: for a proxy in a container
that is its address on the Docker network, not the host's public address.
List a whole network range only if nothing but the proxy can reach the
app's port from it (the port is not also published on the host's public
interfaces); otherwise list the proxy's own address. Without
`HOS_TRUSTED_PROXIES` the headers count from every peer, as in earlier
versions; the server then logs a warning at start-up. Firewalling the
port to the proxy (or to the CDN's ranges) remains advisable either way.

## HTTP timeouts

| Phase | Variable | Default | When exceeded |
|---|---|---|---|
| Receiving the request headers | `HOS_HTTP_HEADER_TIMEOUT_SECS` | 120 s | connection closed |
| Receiving the request body | `HOS_HTTP_BODY_TIMEOUT_SECS` | 60 s | `408 Request Timeout`, connection closed |
| Producing the response | `HOS_HTTP_REQUEST_TIMEOUT_SECS` | 120 s | `503`, the request's work is stopped |

The body is read in full (up to `HOS_MAX_BODY_BYTES`) before a request is
handled, so a client that sends its body slowly holds only its own
connection. The header timeout also closes keep-alive connections that stay
idle that long; keep it above the idle time your reverse proxy keeps
backend connections for (Traefik: 90 s), or a proxy may reuse a connection
the app has just closed. Long-lived responses are not affected: the VM
console websocket runs outside these limits. `0` switches a limit off.

## Outbound connections (SSRF guard)

Administrators type hosts and URLs that the server then connects to:
hypervisor `api_url`, panel `host`, SMTP `host`, registrar `base_url`, RDAP
`bootstrap_url`, AI `base_url`. `HOS_OUTBOUND_POLICY` decides where those may
point. It is read once at start-up:

| Value | Allowed | Use when |
|---|---|---|
| `private` (default) | public addresses **and** private ranges (10/8, 172.16/12, 192.168/16, 100.64/10 CGNAT, fc00::/7) | the hosting company reaching its own Proxmox / vCenter / panels on the LAN |
| `public` | public unicast addresses only | integrations should never reach the server's own network (and always with several unrelated companies, see below) |

Both values always refuse loopback (`127.0.0.0/8`, `::1`, `localhost`),
link-local (`169.254.0.0/16`, `fe80::/10`) and cloud metadata addresses
(`169.254.169.254`, `fd00:ec2::254`, `100.100.100.200`, …), unspecified,
multicast, broadcast and reserved addresses, in any notation (decimal / octal
IPv4, IPv4-mapped IPv6). An unknown value stops `hostingos` at start-up
with an error (a process that reads it otherwise falls back to `public`).
The same classifier guards outgoing webhooks, which may only reach public
addresses whatever the policy.

`docker-compose.yml`, `docs/deploy/env.example` and `docs/deploy/install.sh`
default to `private`.

* Every DNS answer is checked when the connection is made, so a name that
  re-resolves to an internal address (DNS rebinding) is still refused.
* Integration HTTP clients do not follow redirects, ignore
  `HTTP(S)_PROXY` (a proxy would resolve names itself and bypass the check)
  and cap response bodies (16 MiB for provider APIs).
* A refused endpoint shows up as a configuration error in "Test connection";
  connection-test results show only a classified error (category, HTTP
  status, provider error code / message), never the remote response body.
* A model server on the same host (e.g. Ollama on `localhost:11434`) is
  refused under both values; run it on a LAN address and keep `private`.

## Mail

Mail goes through the SMTP / mail integration configured in the admin panel
(queued as jobs, retried). Without one, the log mailer writes sender,
recipients, subject and sizes to the log and sends nothing. Bodies carry
secrets (confirmation and password-reset links), so they reach the log only
with `HOS_DEBUG_MAIL=1` (development; never in production).

## Backups

* Database: `pg_dump --format=custom --no-owner "$DATABASE_URL_DIRECT" > hostingos-$(date +%F).dump`
  daily, keep several generations off-host; test a restore regularly.
* `HOS_SECRET_KEY`: store it separately (password manager / KMS). A dump
  without the key restores everything except integration credentials and
  TOTP secrets.
* Nothing else is stateful: ticket attachments live in the database;
  Caddy's certificate volume can be re-issued.

Compose example:

```sh
docker compose exec -T postgres pg_dump -U hostingos -Fc hostingos > hostingos-$(date +%F).dump
```

## Scaling

* **Web instances**: stateless except for the **in-memory rate limiters**
  (login, sign-up, password reset, 2FA, domain search and the per-key API
  limit). With N instances the effective limit is up to N × the configured
  value and the `RateLimit-*` headers describe one instance. Use sticky
  routing by client IP, or keep a single web instance, until a shared
  limiter (PostgreSQL / Redis) is added. Sessions, idempotency keys and jobs
  are in PostgreSQL and work across instances.
* **Workers**: run web with `serve --no-worker` and one or more
  `hostingos worker` processes (compose: `docker compose --profile split up -d`
  and set `command: ["serve", "--no-worker"]` on `app`). Jobs are claimed with
  `FOR UPDATE SKIP LOCKED`, so several workers are safe; the cron scheduler
  enqueues each tick idempotently. Tune `HOS_WORKER_CONCURRENCY` per process.
* **Database**: point `DATABASE_URL` at a pooler (PgBouncer / Neon pooler,
  transaction mode); keep `DATABASE_URL_DIRECT` on a direct connection for
  migrations and `LISTEN/NOTIFY`.

## Moving from Neon to your own PostgreSQL

1. Provision PostgreSQL 16+ and create the database and role.
2. Stop writes: stop the app and workers (or put the reverse proxy in
   maintenance mode).
3. Dump from Neon's **direct** (non-pooler) endpoint:
   ```sh
   pg_dump --format=custom --no-owner --no-privileges \
     "postgres://USER:PASS@ep-xxx.REGION.aws.neon.tech/DB?sslmode=require" \
     > neon.dump
   ```
4. Restore:
   ```sh
   pg_restore --no-owner --no-privileges --jobs=4 \
     --dbname "postgres://hostingos:PASS@db.internal:5432/hostingos" neon.dump
   ```
   (Use a `pg_dump` client version ≥ the server versions involved.)
5. Run `hostingos migrate` against the new database (it should report
   nothing pending) and compare a few row counts (`clients`, `invoices`,
   `jobs`).
6. Set `DATABASE_URL` / `DATABASE_URL_DIRECT` to the new server, keep the same
   `HOS_SECRET_KEY`, start the app and check `/readyz`.
7. Keep the Neon branch read-only for a while as a fallback, then delete it.

## Advanced: multi-company mode

`HOS_MODE=multi` runs several hosting companies on one installation, each
at `<slug>.<HOS_BASE_DOMAIN>` (or a DNS-verified custom domain), with an
operator console at `<HOS_BASE_URL>/platform` (companies, plans, sign-up,
updates). It is meant for running HostingOS as a service and for
development; a hosting company running its own business needs none of it.
Its additional settings are `HOS_BASE_DOMAIN`, `HOS_PUBLIC_IPS` and, in
debug mode, `HOS_DEV_TENANT`; the console account comes from the hidden
`hostingos create-platform-admin` command. With several unrelated companies
set `HOS_OUTBOUND_POLICY=public` (the server refuses to start with
self-service sign-up open under `private`). Nothing in this mode changes
the single-company behaviour described above.
