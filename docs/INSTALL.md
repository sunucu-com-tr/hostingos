# Installing HostingOS

**English** · [Türkçe](INSTALL.tr.md)

This guide installs HostingOS for one hosting company (its storefront,
client area and admin panel) in one of two ways:

- **A. Docker Compose** on one Linux server: PostgreSQL, HostingOS and
  Caddy, which terminates TLS and obtains certificates automatically.
- **B. Bare binary under systemd**, behind a reverse proxy, with the
  release layout that lets HostingOS install updates in place.

Both run the same `hostingos` binary: web server, background job worker,
scheduler and admin CLI in one program. Templates and database migrations
are compiled in; `static/` and `locales/` are read from disk next to the
binary. Scaling and a separate worker process are covered in
[DEPLOYMENT.md](DEPLOYMENT.md), which also describes multi-company
operation, an optional advanced mode.

To look around on your own computer first, without a server, DNS or TLS,
use the quick start in the [README](../README.md#try-it-on-your-computer)
(`docker-compose.quickstart.yml`).

> **Status.** HostingOS has not had a tagged release yet, so there are no
> signed release archives or versioned images. Compose (way A) runs the
> prebuilt image `ghcr.io/sunucu-com-tr/hostingos:edge`, which CI builds
> from the `main` branch after all tests pass: it follows `main` and is not
> a signed release. It is available after the first image has been
> published; until then, build it from source (section 5). Way B builds
> the binary from source until the first signed release.

## Contents

1. [Requirements](#1-requirements)
2. [Host names and DNS](#2-host-names-and-dns)
3. [Secrets](#3-secrets)
4. [Outbound connections: `HOS_OUTBOUND_POLICY`](#4-outbound-connections-hos_outbound_policy)
5. [A. Docker Compose with Caddy](#5-a-docker-compose-with-caddy)
6. [B. Bare binary under systemd](#6-b-bare-binary-under-systemd)
7. [Reverse proxy and client addresses](#7-reverse-proxy-and-client-addresses)
8. [E-mail](#8-e-mail)
9. [First run: your company and the owner account](#9-first-run-your-company-and-the-owner-account)
10. [Environment variables](#environment-variables)
11. [Backups](#backups)
12. [Upgrading](#upgrading)
13. [Troubleshooting](#troubleshooting)

## 1. Requirements

- A Linux server. Release binaries will be built for x86_64 and aarch64
  (musl, static); a source build runs wherever Rust and PostgreSQL do.
- **PostgreSQL 16.** The Compose file brings its own; for a bare-binary
  install use your distribution's package or a managed database (with a
  pooler such as PgBouncer or Neon's, put the pooled address in
  `DATABASE_URL` and the direct one in `DATABASE_URL_DIRECT`).
- A domain whose DNS you control, and ports 80 and 443 reachable from the
  internet if Caddy is to obtain certificates.
- `openssl` to generate secrets.
- For A: Docker Engine with the Compose plugin. Compose downloads the
  published image. Building the image yourself instead compiles
  HostingOS, which takes a while and needs enough memory for the Rust
  compiler.
- For B: systemd, a reverse proxy (the examples use Caddy 2), and either
  `minisign` (to install a signed release with `install.sh`) or a Rust
  toolchain and a C compiler (to build from source). For backups before
  in-place updates, `pg_dump` on the `PATH`.

## 2. Host names and DNS

`HOS_BASE_URL` is the public address of your site, for example
`https://www.example.com`. HostingOS uses it for links in e-mails and
documents, and an `https` address turns on Secure cookies.

The storefront is at `/`, the client area at `/client`, the admin panel at
`/admin`, the REST API at `/api/v1`; `/healthz` (liveness) and `/readyz`
(database reachable and migrated) answer for monitoring. A request whose `Host` header
is malformed (anything but letters, digits, `-` and `.`, an optional port,
or a bracketed IPv6 address) is answered with `400 Bad request`.

HostingOS runs one company (`HOS_MODE=single`, the default): every host
name that reaches the server shows that company, and there is no "site not
found" page. Certificates are a different matter: before Caddy requests
one, it asks HostingOS (`/internal/tls-allow`), and HostingOS agrees only
for the host of `HOS_BASE_URL`. Nobody can make the server request
certificates for arbitrary names. `HOS_BASE_DOMAIN` is not needed.

**DNS.** Point the host of `HOS_BASE_URL` at the server with an A/AAAA (or
CNAME) record. If the site should also be reachable under another name,
for example the bare `example.com`, redirect that name to `HOS_BASE_URL`
at your DNS provider or with an extra site block in the Caddyfile, such as
`example.com { redir https://www.example.com{uri} permanent }`.

## 3. Secrets

| Secret | Generate with | Notes |
|---|---|---|
| `HOS_SECRET_KEY` | `openssl rand -base64 32` | Must decode to exactly 32 bytes (standard or URL-safe base64). Encrypts integration credentials, two-factor secrets and other stored secrets. **Back it up separately from the database.** Losing or changing it makes those values unreadable |
| `POSTGRES_PASSWORD` (Compose) | `openssl rand -hex 32` | Hex keeps it safe inside the connection URL |
| `HOS_TLS_ASK_SECRET` | `openssl rand -hex 32` | Shared between Caddy and HostingOS for the certificate check |

Never commit a filled-in environment file. Empty values count as unset.

## 4. Outbound connections: `HOS_OUTBOUND_POLICY`

Your staff enter hosts and URLs in the admin panel that the server then
connects to: hypervisor and panel API addresses, SMTP servers, registrar
and RDAP endpoints, AI endpoints. `HOS_OUTBOUND_POLICY` decides where
those may point. It is read once at start-up:

| Value | Integrations may reach | Choose it when |
|---|---|---|
| `private` (default) | public addresses **and** private ranges (10/8, 172.16/12, 192.168/16, 100.64/10, fc00::/7) | your Proxmox, vCenter, hosting panels or mail server sit on your own private network |
| `public` | public addresses only | every system HostingOS talks to is reachable on a public address; this keeps integration settings from ever pointing into your internal network |

Both values always refuse loopback, link-local and cloud metadata
addresses, in any notation, and check every DNS answer when the connection
is made. Outgoing webhooks may only reach public addresses whatever the
policy. Any other value stops `hostingos` at start-up. More detail:
[DEPLOYMENT.md](DEPLOYMENT.md#outbound-connections-ssrf-guard).

## 5. A. Docker Compose with Caddy

`docker-compose.yml` runs four services: `postgres` (PostgreSQL 16),
`migrate` (applies pending migrations, then exits), `app` (HostingOS: web
server, job worker and scheduler) and `caddy` (TLS on ports 80 and 443,
configured by `docs/deploy/Caddyfile`). The containers run with a
read-only root filesystem, no capabilities and `no-new-privileges`; the
application runs as uid 10001. `migrate` and `app` run the published image
(see "Image and tags" below); the file never builds one.

```sh
git clone https://github.com/sunucu-com-tr/hostingos.git hostingos
cd hostingos
cp docs/deploy/env.example .env.production
chmod 600 .env.production

sed -i \
  -e "s|^POSTGRES_PASSWORD=.*|POSTGRES_PASSWORD=$(openssl rand -hex 32)|" \
  -e "s|^HOS_SECRET_KEY=.*|HOS_SECRET_KEY=$(openssl rand -base64 32)|" \
  -e "s|^HOS_TLS_ASK_SECRET=.*|HOS_TLS_ASK_SECRET=$(openssl rand -hex 32)|" \
  .env.production
```

Then edit `.env.production`:

| Variable | Example | Meaning |
|---|---|---|
| `HOS_BASE_URL` | `https://www.example.com` | public address of your site |
| `ACME_EMAIL` | `you@example.com` | account e-mail for Let's Encrypt |
| `HOS_OUTBOUND_POLICY` | `private` or `public` | see section 4 |
| `HOS_UPDATE_REPO` | `sunucu-com-tr/hostingos` | where to look for new releases (optional; checks are off while empty) |
| `HOS_MODE` | `single` | already set in the template: one hosting company. Leave it; `HOS_BASE_DOMAIN` stays commented out |

Start everything (the first start downloads the images):

```sh
docker compose --env-file .env.production up -d
docker compose --env-file .env.production ps
docker compose --env-file .env.production logs -f app
```

`migrate` runs before `app` starts, and Caddy waits until the application
reports ready (the container health check runs
`hostingos healthcheck --path /readyz`). Caddy requests a certificate on the first TLS
connection for a host name, after asking HostingOS
(`/internal/tls-allow`) whether that name belongs to the installation, so
nobody can make the server request certificates for arbitrary names.

Continue with [section 9](#9-first-run-your-company-and-the-owner-account);
in this setup every `hostingos …` command is run as

```sh
docker compose --env-file .env.production run --rm app <command> [options]
```

**Image and tags.** `HOS_IMAGE` and `HOS_TAG` in the env file choose the
image; the defaults give `ghcr.io/sunucu-com-tr/hostingos:edge`.

| Tag | What it is |
|---|---|
| `edge` | The newest build of the `main` branch. CI builds and publishes it on every push to `main` once all tests and the end-to-end test have passed. It moves with `main` and is **not a signed release** |
| `sha-<commit>` | The same build under the first seven characters of its commit, for example `sha-0123abc`; never moves. Set `HOS_TAG` to one to stay on that build |
| `X.Y.Z`, `X.Y`, `latest` | Signed, versioned releases. **None has been published yet** |

Compose pulls the image only when it is not on the host yet; a newer
`edge` arrives with `docker compose --env-file .env.production pull`
([Upgrading](#upgrading)). If the pull fails, nothing is built in its
place: the start stops with the registry's error.

**Building from source.** For changed code, or to avoid the published
image, add `docker-compose.build.yml`, which builds the image from your
checkout as `hostingos:local` and never pulls it. It has to be part of
every command of that installation; the simplest way is to set it for the
shell:

```sh
export COMPOSE_FILE=docker-compose.yml:docker-compose.build.yml
# optional: commit and build date for the server page
export HOS_BUILD_COMMIT="$(git rev-parse HEAD)" SOURCE_DATE_EPOCH="$(git log -1 --format=%ct)"
docker compose --env-file .env.production up -d --build
```

**Another proxy or a CDN in front.** Caddy is set up as the only proxy
(`HOS_TRUST_PROXY=true`, one hop; set `HOS_TRUSTED_PROXIES` to the Compose
network's range so only Caddy can set the client address, section 7). With a CDN in front of Caddy, see
[section 7](#7-reverse-proxy-and-client-addresses). To use an existing
reverse proxy instead of Caddy, adapt `docker-compose.yml`: the proxy must
pass the original `Host` header, set `X-Forwarded-For` and provide
certificates for your host names (it can ask
`/internal/tls-allow?domain=<host>` with the header
`X-HostingOS-Secret: <HOS_TLS_ASK_SECRET>`).

## 6. B. Bare binary under systemd

The layout below is what in-place updates expect:

```text
/opt/hostingos/
  releases/<version>/{hostingos, static/, locales/}
  current  -> releases/<version>        (systemd runs current/hostingos)
  previous -> releases/<older version>  (after an update)
  state/                                (update marker, lock, backups)
/etc/hostingos/hostingos.env            (configuration, 0640 root:hostingos)
```

### 6.1 PostgreSQL

Create a role and a database, for example on a local PostgreSQL 16:

```sh
sudo -u postgres createuser --pwprompt hostingos
sudo -u postgres createdb --owner hostingos hostingos
```

The connection string is then
`postgres://hostingos:PASSWORD@127.0.0.1:5432/hostingos`.

### 6.2 The binary

**From a signed release** (once releases exist).
[`docs/deploy/install.sh`](deploy/install.sh) downloads the archive for
your architecture, verifies its minisign signature against the public key
you pass, checks the archive, creates the `hostingos` system user and the
layout above, installs the systemd unit and writes an environment file
template. It never starts the service or runs migrations. Download the
script, read it, then run it:

```sh
sudo ./install.sh --version X.Y.Z --repo sunucu-com-tr/hostingos --pubkey hostingos-release.pub
```

`hostingos-release.pub` is the project's minisign public key. With
`--archive FILE` (signature in `FILE.minisig`) it installs from local files
instead of downloading.

**From source.** Build on a machine with Rust and a C compiler, then
create the same layout by hand (replace `0.1.0` with the version that
`./target/release/hostingos version` prints):

```sh
cargo build --release --locked -p hos-web --bin hostingos

V=0.1.0
sudo useradd --system --home-dir /opt/hostingos --no-create-home \
  --shell /usr/sbin/nologin hostingos
sudo install -d -m 0755 -o hostingos -g hostingos /opt/hostingos /opt/hostingos/releases
sudo install -d -m 0700 -o hostingos -g hostingos /opt/hostingos/state
sudo install -d -m 0755 /opt/hostingos/releases/$V
sudo install -m 0755 target/release/hostingos /opt/hostingos/releases/$V/hostingos
sudo cp -R static locales /opt/hostingos/releases/$V/
sudo chown -R hostingos:hostingos /opt/hostingos/releases/$V
sudo ln -sfn releases/$V /opt/hostingos/current
sudo chown -h hostingos:hostingos /opt/hostingos/current
```

### 6.3 Configuration

```sh
sudo install -d -m 0750 -o root -g hostingos /etc/hostingos
sudo install -m 0640 -o root -g hostingos /dev/null /etc/hostingos/hostingos.env
sudoedit /etc/hostingos/hostingos.env
```

(`install.sh` creates both and leaves an existing file alone.) A minimal
file:

```sh
DATABASE_URL=postgres://hostingos:PASSWORD@127.0.0.1:5432/hostingos
DATABASE_URL_DIRECT=postgres://hostingos:PASSWORD@127.0.0.1:5432/hostingos
HOS_SECRET_KEY=            # openssl rand -base64 32
HOS_BIND=127.0.0.1:8080
HOS_BASE_URL=https://www.example.com
HOS_TRUST_PROXY=true
HOS_TRUSTED_PROXIES=127.0.0.1,::1   # the reverse proxy on this host
HOS_TLS_ASK_SECRET=        # openssl rand -hex 32
HOS_OUTBOUND_POLICY=private
HOS_UPDATE_REPO=
# HOS_MODE=single          # the default: one hosting company
```

`HOS_BIND=127.0.0.1:8080` keeps the application reachable only through the
reverse proxy on the same host. The unit sets `HOS_UPDATE_ROOT` and
`HOS_LOG_FORMAT=json` itself.

### 6.4 Migrations, owner account, service

```sh
sudo -u hostingos sh -c 'set -a; . /etc/hostingos/hostingos.env; exec /opt/hostingos/current/hostingos migrate'
sudo -u hostingos sh -c 'set -a; . /etc/hostingos/hostingos.env; exec /opt/hostingos/current/hostingos setup --email you@example.com --company "Example Hosting"'

sudo install -m 0644 docs/deploy/hostingos.service /etc/systemd/system/hostingos.service   # install.sh did this already
sudo systemctl daemon-reload
sudo systemctl enable --now hostingos
journalctl -u hostingos -f
```

Other `hostingos` commands in this setup are run the same way as
`migrate` above. [`docs/deploy/hostingos.service`](deploy/hostingos.service)
runs `current/hostingos serve` as the `hostingos` user with a hardened
sandbox (read-only system, writes only below `/opt/hostingos`). It restarts
the service when it exits with code **75**, which means "restart me into
the active release" after an update or a rollback; keep
`RestartForceExitStatus=75` if you write your own unit. `SIGTERM` lets
running requests and jobs finish (up to 45 seconds).

### 6.5 Caddy on the same host

Install Caddy 2 from your distribution or the Caddy project and use a
Caddyfile like this (the repository's `docs/deploy/Caddyfile` is the
Compose variant, which reaches the application as `app:8080`):

```caddyfile
{
	email you@example.com
	on_demand_tls {
		ask http://127.0.0.1:8080/internal/tls-allow?secret=YOUR_HOS_TLS_ASK_SECRET
	}
}

https:// {
	tls {
		on_demand
	}
	encode zstd gzip
	reverse_proxy 127.0.0.1:8080 {
		header_up X-Forwarded-Proto {scheme}
	}
}

http:// {
	redir https://{host}{uri} permanent
}
```

## 7. Reverse proxy and client addresses

The client address is used for rate limits, the audit log, sessions and
orders. Behind a reverse proxy the application sees only the proxy's
address unless you tell it to trust the forwarded headers:

| Variable | Default | Meaning |
|---|---|---|
| `HOS_TRUST_PROXY` | off | `true` (or `1`, `yes`, `on`): read the client address from the forwarded headers. With an `https` `HOS_BASE_URL` every request is treated as https; with an `http` one, `X-Forwarded-Proto` is honoured |
| `HOS_TRUSTED_PROXY_HOPS` | `1` | How many proxies in front of the application append to `X-Forwarded-For`; the client address is the entry that many places from the right. `0` ignores the header |
| `HOS_CLIENT_IP_HEADER` | unset | With `HOS_TRUST_PROXY`: a header set by the outermost proxy that carries the visitor's address, for example `CF-Connecting-IP` behind Cloudflare. Used before `X-Forwarded-For` when it holds exactly one valid address |
| `HOS_TRUSTED_PROXIES` | unset | Comma-separated addresses or CIDR ranges (IPv4 and IPv6) of the proxies that connect to the application. With `HOS_TRUST_PROXY` the headers above and `X-Forwarded-Proto` count only on connections from these addresses; anyone else is taken at their own address. Unset: every connection is believed, as before, with a warning at start-up. An entry that is not an address or range stops `hostingos` at start-up |

Typical settings:

- Caddy (or nginx) only, on the same host: `HOS_TRUST_PROXY=true`,
  `HOS_TRUSTED_PROXY_HOPS=1`, `HOS_TRUSTED_PROXIES=127.0.0.1,::1`.
- Caddy in the Compose network: the same, with `HOS_TRUSTED_PROXIES` set
  to the network's range (`docker network inspect <network> --format
  '{{range .IPAM.Config}}{{.Subnet}} {{end}}'`; for this repository's
  `docker-compose.yml` the network is `hostingos_default`).
- A CDN in front of Caddy (CDN → Caddy → HostingOS): Caddy (like Traefik)
  ignores the `X-Forwarded-For` it receives unless the sender is listed in
  its `trusted_proxies`. Either list the CDN's address ranges there and set
  `HOS_TRUSTED_PROXY_HOPS=2`, or let the application read the CDN's own
  header, for Cloudflare `HOS_CLIENT_IP_HEADER=CF-Connecting-IP`.

Turn `HOS_TRUST_PROXY` on only when clients cannot reach the application
directly (bind it to `127.0.0.1`, or keep it inside the Compose network),
or list the proxies in `HOS_TRUSTED_PROXIES`; otherwise anyone who reaches
the port can claim any address. Set `HOS_CLIENT_IP_HEADER` only
when the server accepts connections from that CDN alone (a firewall that
allows only the CDN's address ranges); otherwise visitors can forge the
header (`HOS_TRUSTED_PROXIES` does not help there: such requests arrive
through your proxy). Without `HOS_TRUST_PROXY` behind a proxy, all visitors
share the proxy's address and therefore one rate limit.

The application also limits slow clients: 120 seconds for the request
headers, 60 seconds for the body (`408`) and 120 seconds for the whole
request (`503`); `HOS_HTTP_HEADER_TIMEOUT_SECS`,
`HOS_HTTP_BODY_TIMEOUT_SECS` and `HOS_HTTP_REQUEST_TIMEOUT_SECS` change
them, `0` switches one off ([DEPLOYMENT.md](DEPLOYMENT.md#http-timeouts)).

## 8. E-mail

Order and invoice mails, password resets and ticket replies are sent
through the mail integration you set up in the admin panel under
Integrations (category Mail: SMTP, Resend, Postmark or Mailgun). The
sender address and the Turkish and English templates are under
Settings → E-mail, where a test mail can be sent. Mail is queued as
background jobs and retried. Until a mail integration exists, e-mails are
only written to the log (sender, recipients, subject and size, no body).

`HOS_DEBUG_MAIL=1` writes message bodies, including password-reset links,
to the log. It is meant for development; never set it in production.

Incoming e-mail for support tickets (a Mailgun inbound route or a raw
message endpoint) is described in [API.md](API.md#incoming-e-mail-ticket-piping).

## 9. First run: your company and the owner account

1. **Migrations.** Compose runs them in the `migrate` service; for B run
   `hostingos migrate` (section 6.4), and `setup` applies them as well.
   They are forward-only and safe to run again.
2. **Company and owner account.** Run `hostingos setup` once (Compose:
   `docker compose --env-file .env.production run --rm app setup …`; B: as
   in section 6.4):

   ```sh
   hostingos setup --email you@example.com --company "Example Hosting" --locale en --currency EUR
   ```

   It applies pending migrations, creates the company and its owner
   account and prints the sign-in address. The password comes from
   `--password` or `HOS_NEW_PASSWORD`; without one a random password is
   generated and printed once. The defaults are `--company "My Hosting
   Company"`, `--locale tr` and `--currency TRY`. Running `setup` again only
   reports that the company exists. A server started before `setup` stays
   up and answers every page with `503`; the company appears within
   seconds after `setup`, without a restart.
3. **Log in** at `https://www.example.com/auth/login`; the admin panel is
   at `/admin`. Turn on two-factor authentication for your own account,
   and under Settings → Security you can require it for every staff
   member.
4. **Configure** payment gateways, servers, registrars and mail under
   Integrations, then products and prices in the catalogue. Add further
   staff accounts under Staff, each with a role.
5. **Server pages.** Only the owner sees Server in the admin menu:
   `/admin/server` shows the version, uptime, database, migrations and
   health; `/admin/server/updates` holds the update checks, installing an
   update, maintenance mode and the deploy webhook. Every action there is
   recorded in the audit log under the owner's name.

## Environment variables

Required variables are marked. Empty values count as unset. The complete
reference is in [DEPLOYMENT.md](DEPLOYMENT.md#environment).

**Database and keys**

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | – (**required**) | PostgreSQL connection (`postgres://` or `postgresql://`); a pooled one is fine. `channel_binding=…` is removed automatically |
| `DATABASE_URL_DIRECT` | `DATABASE_URL` | Direct connection for migrations and the worker's `LISTEN/NOTIFY`; set it when `DATABASE_URL` goes through a pooler |
| `HOS_SECRET_KEY` | – (**required**) | 32 random bytes, base64 (section 3) |

**Site and HTTP**

| Variable | Default | Meaning |
|---|---|---|
| `HOS_BASE_URL` | `http://localhost:<port>` | Public address of your site (section 2); `https` turns on Secure cookies |
| `HOS_MODE` | `single` | One hosting company, shown on every host name that reaches the server. An unknown value stops `hostingos` at start-up |
| `HOS_BIND` | `127.0.0.1:8080` | Listen address; the image sets `0.0.0.0:8080` |
| `HOS_TRUST_PROXY`, `HOS_TRUSTED_PROXY_HOPS`, `HOS_CLIENT_IP_HEADER`, `HOS_TRUSTED_PROXIES` | off, `1`, –, – | Section 7 |
| `HOS_TLS_ASK_SECRET` | – | Secret for `/internal/tls-allow` when the proxy is not on loopback |
| `HOS_MAX_BODY_BYTES` | `2097152` | Request body limit in bytes |
| `HOS_HTTP_HEADER_TIMEOUT_SECS`, `HOS_HTTP_BODY_TIMEOUT_SECS`, `HOS_HTTP_REQUEST_TIMEOUT_SECS` | `120`, `60`, `120` | Seconds for the request headers, the body (`408`) and the whole request (`503`); `0` = no limit (section 7) |
| `HOS_MIGRATE_ON_START` | off | `1`: `hostingos serve` applies pending migrations before it opens its port, as `serve --migrate` ([DEPLOYMENT.md](DEPLOYMENT.md#migrations)) |
| `HOS_STATIC_DIR`, `HOS_LOCALES_DIR` | next to the binary, else `./static`, `./locales` | Asset and translation directories |

**Integrations and background work**

| Variable | Default | Meaning |
|---|---|---|
| `HOS_OUTBOUND_POLICY` | `private` | `private` or `public` (section 4) |
| `HOS_WORKER_CONCURRENCY` | `8` | Parallel jobs per worker process (1–256) |
| `HOS_JOB_QUEUE_NAMESPACE` | – | Only for a second installation (staging) that shares the same database (see DEPLOYMENT.md) |
| `ANTHROPIC_API_KEY`, `HOS_AI_MODEL` | –, built-in default | Optional AI drafts for content and SEO; off without a key |
| `GOOGLE_CLIENT_ID` + `GOOGLE_CLIENT_SECRET`, `GITHUB_CLIENT_ID` + `GITHUB_CLIENT_SECRET` | – | OAuth sign-in with Google or GitHub (set both of a pair or neither) |

**Updates** ([UPDATES.md](UPDATES.md))

| Variable | Default | Meaning |
|---|---|---|
| `HOS_UPDATE_REPO` | – | GitHub `owner/name` whose releases carry the signed manifest; update checks are off until this or the next one is set |
| `HOS_UPDATE_MANIFEST_URL` | – | HTTPS URL of a manifest instead of a GitHub repository |
| `HOS_UPDATE_ALLOWED_HOSTS` | GitHub hosts | Extra download hosts |
| `HOS_UPDATE_HEALTH_TIMEOUT` | `300` | Seconds a new release has to become healthy before it is rolled back |
| `HOS_UPDATE_BACKUP_COMMAND` | `pg_dump` if on the `PATH` | Command run before an in-place update |
| `HOS_INSTALL_TYPE`, `HOS_UPDATE_ROOT`, `HOS_SUPERVISED` | detected | Overrides; see UPDATES.md |

**Logging**

| Variable | Default | Meaning |
|---|---|---|
| `HOS_LOG_FORMAT` | text (`json` in the image and the systemd unit) | `json` for structured logs |
| `RUST_LOG` | `info` | Log filter, for example `info,sqlx=warn` |

**Development, demo and CLI only.** Never set the first three in
production.

| Variable | Meaning |
|---|---|
| `HOS_DEBUG` | Development mode (detailed errors, mock integrations allowed). On by default only in debug builds |
| `HOS_DEBUG_MAIL` | Write e-mail bodies to the log |
| `HOS_ALLOW_MOCK_INTEGRATIONS` | Allow the `mock` gateway, hypervisor, panel and registrar (a mock gateway lets anyone pay any invoice) |
| `HOS_DEMO_MODE`, `HOS_DEMO_RESET_MINUTES`, `HOS_DEMO_ALLOW_EXISTING_DATA` | Public demo mode ([DEMO.md](DEMO.md)) |
| `HOS_NEW_PASSWORD` | Password for an account created on the command line; without it a random password is printed |
| `HOS_IMPORT_DB_PASSWORD`, `HOS_IMPORT_CC_HASH`, `HOS_IMPORT_API_SECRET`, `HOS_IMPORT_API_ACCESS_KEY` | Secrets for `hostingos import whmcs` ([IMPORT-WHMCS.md](IMPORT-WHMCS.md)) |

**Compose file only:** `POSTGRES_PASSWORD`, `ACME_EMAIL`, `HOS_IMAGE`
(default `ghcr.io/sunucu-com-tr/hostingos`), `HOS_TAG` (default `edge`;
section 5).

## Backups

- **Database.** Dump it daily and keep several generations on another
  machine; test a restore now and then.

  ```sh
  # Compose
  docker compose --env-file .env.production exec -T postgres \
    pg_dump -U hostingos -Fc hostingos > hostingos-$(date +%F).dump

  # Bare binary (any host with pg_dump and access to the database)
  pg_dump --format=custom --no-owner "$DATABASE_URL_DIRECT" > hostingos-$(date +%F).dump
  ```

- **`HOS_SECRET_KEY`.** Store it apart from the dumps, for example in a
  password manager. A dump restored without the key loses the integration
  credentials and two-factor secrets.
- **Nothing else is stateful.** Ticket attachments and uploads are in the
  database; Caddy's certificates are issued again if lost.
- In-place updates of a bare-binary install take their own backup first
  (`HOS_UPDATE_BACKUP_COMMAND`, otherwise `pg_dump` into
  `/opt/hostingos/state/backups/`).

To restore, stop the application, restore into an empty database with
`pg_restore --no-owner --dbname "<connection string>" <file>`, start
HostingOS with the same `HOS_SECRET_KEY` and check `/readyz`.

## Upgrading

Read [CHANGELOG.md](../CHANGELOG.md) and take a backup first. Migrations
only move forward and are applied before the new version serves requests.

- **Compose with the published image** (the default): change `HOS_TAG` if
  you pinned a `sha-…` build, then

  ```sh
  docker compose --env-file .env.production pull
  docker compose --env-file .env.production up -d
  ```

  `up` recreates the containers whose image changed: the one-shot
  `migrate` service applies the new version's migrations first, and `app`
  starts only after it has finished successfully. If a migration fails,
  `app` is not started and `docker compose … logs migrate` shows why. A
  container never replaces itself. Signed releases (once they exist) will
  also be announced on the update page, which can call a deploy webhook;
  `edge` builds are not announced there.
- **Compose, built from source:** `git pull`, then
  `docker compose --env-file .env.production up -d --build` with
  `COMPOSE_FILE` set as in section 5.
- **Bare binary:** set `HOS_UPDATE_REPO`, then install the new release
  as the owner in the admin panel under Server → Updates
  (`/admin/server/updates`), which asks for your password again (and the
  two-factor code when it is on) and takes care of the backup,
  maintenance mode, migrations, health check and automatic rollback; or
  run `hostingos update check` and `hostingos update apply <version>`.
  `hostingos update rollback` points `current` back to the previous release
  and leaves the database as it is. `install.sh --version … --activate`
  switches manually.

In-place installs refuse releases that are not signed with a trusted key.
No signing key exists yet, so this path works from the first signed
release on. Details: [UPDATES.md](UPDATES.md).

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| `missing required environment variable …` / `invalid environment variable HOS_SECRET_KEY` | The variable is empty or the key does not decode to 32 bytes; generate it with `openssl rand -base64 32` |
| Pulling `ghcr.io/sunucu-com-tr/hostingos` fails with `denied`, `unauthorized` or `not found` | The image has not been published yet, or is not public yet, or `HOS_TAG` names a tag that does not exist. Check the tag, or build from source (section 5) |
| `invalid HOS_OUTBOUND_POLICY value` | Only `private` and `public` are accepted |
| Every page answers `503` and the log says "no hosting company has been set up" | Run `hostingos setup` (section 9); no restart is needed |
| The server refuses to start: "this database holds … hosting companies" | The database contains more than one company (for example after a test); point `DATABASE_URL` at your company's database |
| `Bad request – Invalid Host header` | The proxy forwards an unusual `Host` (for example with `_`); pass the original host name |
| Login does not stick | With an `https` `HOS_BASE_URL`, cookies are Secure and only sent over HTTPS; open the site through the proxy's https address |
| Every visitor has the same IP address, or rate limits hit everyone at once | `HOS_TRUST_PROXY` is off behind a proxy, or the proxy settings do not match your proxies (section 7) |
| No certificate for a host name | DNS must point at the server and ports 80/443 must be open. Caddy only gets certificates for names that `/internal/tls-allow` approves (section 2). A `403` in Caddy's log means `HOS_TLS_ASK_SECRET` differs between Caddy and HostingOS. `docker compose … logs caddy` shows details |
| "Test connection" of an integration reports a configuration error | The address is refused by the outbound policy (loopback, link-local, metadata, or a private address under `public`); see section 4 |
| No e-mails arrive | No mail integration is set up yet, so mail is only logged (section 8) |
| `mock` providers cannot be saved | They are refused in production. They are for development and demos only (`HOS_ALLOW_MOCK_INTEGRATIONS`, debug or demo mode) |
| `/readyz` fails | `"database":"error"`: the database is not reachable with `DATABASE_URL`; `"schema":"behind"`: migrations of this version are not applied, run `hostingos migrate` (no restart needed) |
| Updates: "Update checks are off", "no supervisor detected", stuck in maintenance mode | See the troubleshooting table in [UPDATES.md](UPDATES.md#7-troubleshooting) |

Logs: `docker compose --env-file .env.production logs -f app` or
`journalctl -u hostingos -f`. More on scaling and a separate worker:
[DEPLOYMENT.md](DEPLOYMENT.md).
