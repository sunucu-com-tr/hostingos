# Deploying the public demo

[Türkçe](DEMO-DEPLOY.tr.md)

How to run a public HostingOS demo (demo mode: [../DEMO.md](../DEMO.md))
on one hostname. Two shapes:

* **A — Coolify, separate PostgreSQL** (recommended; the shape the public
  demo uses). The application either runs the published image (nothing is
  compiled on the server; recommended) or is built from the Dockerfile.
* **B — `docker-compose.demo.yml`**: app and its own PostgreSQL in one
  stack, on Coolify or any Docker host.

To look at the demo on your own computer instead, no server is needed:
see "Quick start" in the [README](../../README.md#try-it-on-your-computer)
(`docker-compose.quickstart.yml` with `HOS_DEMO_MODE=1`).

**The published image.** `ghcr.io/sunucu-com-tr/hostingos:edge` is built
by CI from the `main` branch after all tests pass; it follows `main` and
is not a signed release (signed, versioned releases are not published
yet). `ghcr.io/sunucu-com-tr/hostingos:sha-<7-digit commit>` is one fixed
build. Both are available after the first image has been published.
Building the Dockerfile instead compiles HostingOS on the server, which
takes a long time and needs a lot of memory on a small server.

`demo.example.invalid` stands for your demo hostname throughout.

## What you get

On `https://demo.example.invalid` (one hostname, no wildcard DNS):

| Path | What |
|---|---|
| `/` | Storefront of the demo hosting company "Demo Hosting A.Ş." (Turkish; English under `/en`) with the demo banner |
| `/auth/login` | Login with one-click demo accounts: `admin@demo.test` / `demo-admin` (admin panel), `musteri@demo.test` / `demo-client` (client area) |
| `/admin`, `/client` | Admin panel and client area |
| `/admin/server` | The owner's server pages (version, health, updates), read-only in the demo |
| `/demo/mailbox` | E-mails the demo "sent" (nothing leaves the server) |

Visitors can add and change content, including what the storefront shows;
everything is deleted at the next reset (default every 30 minutes). The
banner on every page and the login pages say so, in Turkish and English.

## Environment variables

| Name | Value | Notes |
|---|---|---|
| `HOS_DEMO_MODE` | `1` | Required: switches demo mode on |
| `HOS_DEMO_RESET_MINUTES` | `30` | Optional (5–1440, default 30) |
| `HOS_MODE` | unset | Leave unset (or `single`, the default): a demo is a single-company installation; `multi` is refused |
| `DATABASE_URL` | `postgres://…` | Required. The demo's **own, empty** database (see below) |
| `DATABASE_URL_DIRECT` | `postgres://…` | Optional; only if `DATABASE_URL` goes through PgBouncer |
| `HOS_SECRET_KEY` | `openssl rand -base64 32` | Required. Changing it later is harmless for the demo |
| `HOS_BASE_URL` | `https://demo.example.invalid` | Required. No trailing slash. Its host serves everything |
| `HOS_TRUST_PROXY` | `true` | A proxy (Coolify's Traefik) is in front |
| `HOS_CLIENT_IP_HEADER` | `CF-Connecting-IP` | Behind Cloudflare; leave unset without a CDN ([Proxies](#proxies-and-the-visitor-ip)) |
| `HOS_TRUSTED_PROXY_HOPS` | `1` | Fallback when the header above is missing |
| `HOS_TRUSTED_PROXIES` | the Docker network's range, e.g. `10.0.1.0/24` | Recommended: the forwarding headers count only from these peers (Traefik). Find the range as in [Proxies](#proxies-and-the-visitor-ip); unset = every peer, with a warning in the log |
| `HOS_MIGRATE_ON_START` | unset | Not needed: demo mode always applies pending migrations before it accepts connections |
| `HOS_OUTBOUND_POLICY` | `public` | Recommended (demo mode makes no outbound connections anyway) |
| `HOS_BIND` | `0.0.0.0:8080` | Already the image default |
| `HOS_DEBUG` | `0` | Already the default of release builds; demo mode refuses `1` |
| `HOS_DEMO_ALLOW_EXISTING_DATA` | unset | Never set it for a public demo |

Nothing else is needed: no wildcard DNS, no `HOS_BASE_DOMAIN`, no custom
domain step, no e-mail or payment credentials. The first start creates the
schema, seeds the demo and makes the `HOS_BASE_URL` host the demo company's
domain.

## The database must be the demo's own

Demo mode deletes and re-creates the demo company every interval, so it
refuses to start on a database that holds anything else (another company,
or an operator account of multi-company mode), with this in the log:

```text
HOS_DEMO_MODE refuses to start: this database holds data that is not the demo's …
```

If the PostgreSQL resource was used by a normal HostingOS installation
before (`hostingos setup` run, a company created), give the demo a new,
empty database — a new PostgreSQL resource in Coolify, or `create database
hostingos_demo;` on the same server and that database in `DATABASE_URL`.
Do not use `HOS_DEMO_ALLOW_EXISTING_DATA=1` to get around it: that keeps
the other data next to a public demo, and a company that already owns the
demo hostname as its domain makes the reset fail.

## A — Coolify, separate PostgreSQL

Coolify facts below are from the Coolify documentation (Docker Image
applications, Dockerfile build pack, Health checks, Deployment commands,
Basic authentication).

1. **PostgreSQL.** Create a PostgreSQL 16 resource (or reuse an empty
   one) and copy its internal connection URL. No backups are needed: the
   demo holds no data worth keeping.
2. **Application**, one of:
   * **From the published image (recommended).** **+ New** → **Docker
     Image** → **Image Name** `ghcr.io/sunucu-com-tr/hostingos:edge`
     (Coolify moves a pasted tag into its own field; a `sha-…` tag or a
     digest pins one build) → **Save**. Nothing is cloned or compiled; each
     deployment pulls the image.
   * **Built from the Dockerfile.** New resource → your Git repository →
     **Build pack: Dockerfile**, **Base Directory** `/`, **Dockerfile
     Location** `/Dockerfile`. Leave **Docker Build Stage Target** empty
     (the last stage builds from source).
3. **Network.** **Ports Exposes** `8080` (a Docker Image application
   starts with `80`: replace it). **Domains**
   `https://demo.example.invalid`.
4. **Environment variables.** As in the table above (`DATABASE_URL` = the
   internal URL from step 1). With the Dockerfile build, set each one's
   **Build time** to **Not available during build**: the image build needs
   none of them, Coolify turns build-time variables into injected `ARG`
   instructions (so changing a value can throw away the cached dependency
   build), and build arguments can show up in image metadata. Under
   **Advanced → Build arguments** choose **Managed manually in
   Dockerfile**, and keep the build cache on.
5. **Health check.** Leave Coolify's **Healthcheck** page *disabled*:
   Coolify then uses the image's `HEALTHCHECK`, which runs
   `/usr/local/bin/hostingos healthcheck` (plain HTTP `GET /healthz` on
   port 8080 inside the container, exit 0 / 1). If you prefer the dashboard
   check, choose **Check type: CMD** and enter
   `/usr/local/bin/hostingos healthcheck` (for a database check:
   `/usr/local/bin/hostingos healthcheck --path /readyz`, which also
   fails while migrations are pending); set **Start period** to at least
   60 seconds. Do **not** choose the HTTP type: it
   needs `curl` or `wget` inside the image, and this image has neither.
   Traefik stops routing to a container whose check fails.
6. **Post-deployment command**: leave it empty. Demo mode applies pending
   migrations *before* the server accepts connections (the same as
   `hostingos serve --migrate`), so no request ever meets a half-migrated
   database. A `/usr/local/bin/hostingos migrate` entered here earlier is
   no longer needed; it does no harm either (it waits for a database lock
   while the server migrates, then finds nothing to do).
7. **Deploy.** The first start migrates, seeds and resets the demo (a few
   seconds), then the health check turns healthy. If a migration fails,
   the container stops with the error in its log instead of serving.
8. **Verify** (from your machine):

   ```sh
   curl -sI https://demo.example.invalid/ | grep -i -e '^HTTP' -e x-robots-tag
   # HTTP/2 200 … x-robots-tag: noindex, nofollow, noarchive
   ```

   Then open the site, sign in with "Hosting firması yöneticisi" on the
   login page and open `/admin/server` (read-only). In Coolify's
   **Terminal**: `/usr/local/bin/hostingos healthcheck` prints
   `ok: HTTP 200 …`.
9. **Remove HTTP basic auth** once the demo works (next section).

**Reset by hand.** Coolify **Terminal** of the application:

```sh
/usr/local/bin/hostingos demo-reset
```

It deletes everything visitors entered and restores the sample data at
once; the running server picks it up within 30 seconds and starts a new
interval. A **Scheduled Task** with the same command and **Execute Now**
works too, and so does **Restart** (every start resets).

**Updating.** Redeploy. With the published image, Coolify pulls the tag
again, so a redeploy of `edge` brings the newest build of `main` (a
**Restart** pulls the saved image reference again too); with a `sha-…` tag,
change the tag first. With the Dockerfile build, a redeploy builds the new
commit. The new container applies pending migrations and resets the demo
before it listens. While it does, a previous container that is still
running keeps serving: a database ahead of its release is logged once and
served (migrations are additive), not refused.

**Moving an existing demo from the Dockerfile build to the image.** Create
the Docker Image application as in step 2 with the same environment
variables and health check, move the domain over to it, deploy, and delete
the old application once the new one answers. The database stays as it is
(the demo resets it anyway).

## Removing HTTP basic auth

Demo mode needs no `Authorization` header: it ignores `Authorization:
Basic …` (sessions are cookies; the CSRF check treats such requests as
cookie-authenticated), so the site works with or without basic auth in
front. But:

* the public cannot see the demo while basic auth is on;
* REST API keys (`Authorization: Bearer …`) cannot be used through it, as
  a request carries only one `Authorization` header (the API is read-only
  in the demo anyway);
* the container health check is not affected (it runs inside the
  container, not through Traefik).

So turn it off when the demo is ready: application → **General** →
**Security > Authentication**, switch it away from **HTTP Basic
Authentication** (or remove the basic-auth labels if you added them by
hand), save, then **Restart** or redeploy so Coolify recreates the
container's proxy labels.

## Proxies and the visitor IP

Rate limits are per visitor IP, so the app must see it. The path is
Cloudflare → Coolify's Traefik → app:

* Coolify starts Traefik without `forwardedHeaders.trustedIPs` (checked
  against Coolify's proxy configuration source), and Traefik then does not
  trust the `X-Forwarded-For` it receives: it replaces it with the address
  of the Cloudflare server that connected. With only
  `HOS_TRUSTED_PROXY_HOPS=1` every visitor would share a few Cloudflare
  addresses and the limits would hit everyone at once; no other HOPS value
  helps, because the visitor's address is no longer in the header.
* Cloudflare also sends `CF-Connecting-IP` with the visitor's address,
  which Traefik passes through. `HOS_CLIENT_IP_HEADER=CF-Connecting-IP`
  makes the app use it (when it holds exactly one valid address), so:

  ```env
  HOS_TRUST_PROXY=true
  HOS_CLIENT_IP_HEADER=CF-Connecting-IP
  HOS_TRUSTED_PROXY_HOPS=1
  ```

  `HOS_TRUSTED_PROXY_HOPS=1` is right as the fallback (Traefik's single
  entry). If you configure Traefik to trust Cloudflare's ranges,
  `X-Forwarded-For` becomes `visitor, cloudflare` and `2` would be the
  matching value; the header setting works either way.
* `HOS_TRUSTED_PROXIES` names the peers whose forwarding headers
  (`CF-Connecting-IP`, `X-Forwarded-For`, `X-Forwarded-Proto`) count; a
  connection from any other address is limited by its own address. Traefik
  reaches the app over the Docker network of the application's
  **Destination** (named `coolify` unless you created another), so list
  that network's range. On the server:

  ```sh
  docker network inspect coolify --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}'
  # prints e.g. 10.0.1.0/24 → HOS_TRUSTED_PROXIES=10.0.1.0/24
  ```

  For a Docker Compose application, list the networks of the `app`
  container instead (`docker inspect <container> --format '{{range $k, $v
  := .NetworkSettings.Networks}}{{$k}} {{end}}'`) and inspect each one the
  same way. Every container on a listed network could set the headers, so
  keep other people's containers off it. With the variable unset the app
  believes every peer, as before, and logs a warning at startup; an entry
  that is not an address or range stops the start with an error.
* Anyone who reaches the server directly, around Cloudflare, still comes
  in through Traefik and can send their own `CF-Connecting-IP`; the list
  above does not stop that. Allow ports 80 and 443 only from
  [Cloudflare's IP ranges](https://www.cloudflare.com/ips/) in the server
  firewall (or use a Cloudflare Tunnel).
* https: nothing to set. With an `https://` `HOS_BASE_URL` the app treats
  requests as https whatever scheme the last hop reports. Keep
  Cloudflare's SSL/TLS mode at **Full (strict)**, as Coolify's Cloudflare
  guide recommends.

Without Cloudflare (Traefik alone): `HOS_TRUST_PROXY=true`,
`HOS_TRUSTED_PROXY_HOPS=1`, `HOS_TRUSTED_PROXIES` as above, no
`HOS_CLIENT_IP_HEADER`.

**Slow clients.** The app closes a connection whose request headers take
longer than 120 s, answers 408 to a body that is not complete in 60 s and
503 to a request still running after 120 s
(`HOS_HTTP_HEADER_TIMEOUT_SECS`, `HOS_HTTP_BODY_TIMEOUT_SECS`,
`HOS_HTTP_REQUEST_TIMEOUT_SECS`; docs/DEPLOYMENT.md). Traefik's own
entrypoint limits (60 s to read a request, 180 s idle by default) apply in
front of these; the defaults need no change for the demo.

## B — docker-compose.demo.yml

The stack: `postgres` (PostgreSQL 16, named volume, not published) and
`app` (built from the Dockerfile unless `HOS_DEMO_IMAGE` names an image,
demo mode, read-only root filesystem, no capabilities, CPU / memory
limits, health check `hostingos healthcheck --path /readyz`).

**On Coolify:** New resource → Git repository → **Build pack: Docker
Compose**, **Base Directory** `/`, **Docker Compose Location**
`/docker-compose.demo.yml`. Coolify lists the variables the file needs:
`POSTGRES_PASSWORD` (`openssl rand -hex 32`), `HOS_SECRET_KEY`,
`HOS_BASE_URL`, and set `HOS_CLIENT_IP_HEADER=CF-Connecting-IP` behind
Cloudflare. Give the `app` service the domain
`https://demo.example.invalid:8080` (for Compose services the port goes at
the end of the domain). Basic auth for a Compose application is done with
labels in the Compose file; remove them when the demo is live. Reset:
**Terminal** of the `app` container, `/usr/local/bin/hostingos demo-reset`.

To run the published image on Coolify without a build, use shape A with a
Docker Image application.

**On a Docker host with its own reverse proxy:**

```sh
cp .env.demo.example .env.demo      # fill in the REQUIRED values
docker compose -f docker-compose.demo.yml -f docker-compose.demo.ports.yml \
  --env-file .env.demo up -d --build --wait
# the app is on 127.0.0.1:8080; point the reverse proxy for
# demo.example.invalid at it
docker compose -f docker-compose.demo.yml --env-file .env.demo exec app \
  /usr/local/bin/hostingos demo-reset            # reset by hand
docker compose -f docker-compose.demo.yml --env-file .env.demo down --volumes
                                                 # remove everything
```

With the published image instead of the build, put
`HOS_DEMO_IMAGE=ghcr.io/sunucu-com-tr/hostingos:edge` into `.env.demo` and
start with `--pull always --no-build` in place of `--build`; a failed pull
then stops with an error instead of building:

```sh
docker compose -f docker-compose.demo.yml -f docker-compose.demo.ports.yml \
  --env-file .env.demo up -d --pull always --no-build --wait
```

The same command updates the demo to the newest image.

## Monitoring and logs

* `GET /readyz` from outside for an uptime monitor: 200 when the database
  is reachable and has every migration of the running release, 503 with
  `"schema":"behind"` while migrations are pending; `/healthz` for
  liveness.
* Logs are JSON (`HOS_LOG_FORMAT=json`). Each reset logs `demo reset` with
  its duration; requests still running after 30 seconds no longer hold it
  up (`demo reset: requests were still running; resetting anyway`); a
  failed reset logs an error and is retried a minute later. Refused
  requests log `demo mode: request refused` at info level.
* The demo has no data to back up. Removing it: delete the application and
  its database.
