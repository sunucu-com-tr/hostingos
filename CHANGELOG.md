# Changelog

All notable changes to HostingOS are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and the project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Every release section becomes the release notes and the `notes_md` of the
signed update manifest (see [docs/RELEASING.md](docs/RELEASING.md)). A
`### Security` heading marks a security release. An optional comment line
sets update metadata, for example:

    <!-- release: rollback_safe=true min_upgradable_from=0.2.0 -->

## [Unreleased]

Nothing has been released yet. The first release will contain the
following.

### Added

- Hosting automation for one hosting company in a single binary
  (`hostingos`) on PostgreSQL: storefront, client area and admin panel on
  the company's own address. `hostingos setup` creates the company and its
  owner. The owner's server pages (`/admin/server`: version, health,
  updates, maintenance mode) need the new `system.manage` permission, which
  only the owner role has. An advanced multi-company mode
  (`HOS_MODE=multi`) runs several companies with an operator console.
- Storefront: products, configurable options, domain search (registrar or
  RDAP), knowledge base, blog and announcements in Turkish and English. SEO
  features: canonical URLs, hreflang, JSON-LD, sitemap, `llms.txt`,
  IndexNow and Markdown twins of pages.
- Cart and checkout: coupons, tax, credit balance. An order leads to an
  invoice, then payment, then automatic provisioning.
- Client area: services (SSO, upgrades, cancellation), VPS/cloud management
  (power, reinstall, snapshots, console, metrics), domains (renewal, name
  servers, lock, privacy, EPP, contacts, DNS), invoices, credit, payment
  methods, support tickets and account security.
- Admin panel: clients, orders, invoices, services, domains, catalogue,
  integrations, e-mail templates, quotes, affiliates, settings, role-based
  access control, audit log, impersonation and global search.
- Billing automation: invoice generation, reminders, suspension and
  termination, domain synchronisation. A PostgreSQL job queue and a cron
  scheduler run in-process or as a separate `hostingos worker`.
- Integrations: Stripe, iyzico, PayTR, PayPal and bank transfer; cPanel/WHM,
  Plesk, DirectAdmin and CyberPanel; Proxmox VE, VMware, Virtualizor and
  Hetzner Cloud; ResellerClub, Namecheap and OpenProvider; SMTP, Resend,
  Postmark and Mailgun; Turkish e-invoice / e-archive integrators. Each
  category has a `mock` provider for development.
- WHMCS import, invoice PDFs and theming.
- REST API (`/api/v1`) with an OpenAPI 3.1 description, API keys,
  idempotency keys, rate-limit headers, and incoming and outgoing webhooks.
- Security: argon2id passwords, CSRF protection, TOTP two-factor
  authentication, OAuth (Google, GitHub), rate limits, credentials
  encrypted at rest and every query scoped to the company.
- Self-update (docs/UPDATES.md):
  - signed release manifests from GitHub Releases or a manifest URL,
    verified with minisign (Ed25519);
  - key rotation, and protection against downgrades and replays;
  - daily jittered checks with in-app and e-mail notifications;
  - one-click install for bare-binary installs, with backup, maintenance
    mode, migrations, health check and automatic rollback;
  - image instructions and an optional deploy webhook for container
    installs;
  - `hostingos update check|status|apply|rollback` and
    `hostingos version --json`.
- Release pipeline: CI (format, lints, tests against PostgreSQL, dependency
  audit). Tag builds produce:
  - static Linux binaries for x86_64 and aarch64;
  - an SBOM, checksums and minisign signatures;
  - a multi-arch container image with provenance;
  - a GitHub Release carrying the signed update manifest.
- Prebuilt container image from `main`: CI publishes
  `ghcr.io/sunucu-com-tr/hostingos:edge` and `:sha-<commit>` after all
  tests and the end-to-end demo test pass (not signed releases).
  `docker-compose.quickstart.yml` runs it with PostgreSQL on
  `http://localhost:8080` for a trial without building, optionally in demo
  mode; `docker-compose.yml` now runs the published image and never builds,
  `docker-compose.build.yml` builds from a checkout instead.
- Deployment: Dockerfile, Docker Compose with Caddy and automatic TLS, a
  systemd unit and an install script that verifies signatures.
  `hostingos healthcheck` is the container health check (no curl or shell
  needed). `HOS_CLIENT_IP_HEADER` reads the visitor address from a CDN
  header such as `CF-Connecting-IP`.
- Public demo mode (`HOS_DEMO_MODE`, docs/DEMO.md): published logins, mock
  integrations only, no outbound connections, a banner with a countdown,
  and a full reset every 30 minutes (configurable, or `hostingos
  demo-reset`). `docker-compose.demo.yml` and runbooks for Coolify.
- Migrations at startup: `hostingos serve --migrate` or
  `HOS_MIGRATE_ON_START=1` applies pending migrations before the port opens
  (always in demo mode), under the same PostgreSQL lock as `hostingos
  migrate`, so both can run at once. While the database is behind the
  running version, pages answer `503` with the maintenance page and
  `/readyz` reports `"schema":"behind"`; once the migrations are in, the
  server serves again without a restart.
- HTTP timeouts for slow clients: `HOS_HTTP_HEADER_TIMEOUT_SECS` (120),
  `HOS_HTTP_BODY_TIMEOUT_SECS` (60, answers `408`) and
  `HOS_HTTP_REQUEST_TIMEOUT_SECS` (120, answers `503`).
- Licensed under the GNU Affero General Public License v3.0 only.
- Storefront template "Atlas" (seventh template) for companies that sell
  servers by specification: a deep brand-tinted hero on an engineering grid
  with a rack drawing of the product groups, a utility bar with phone and
  service status, spec-sheet plans with tabular figures, ruler section
  heads and a deep footer. Pure CSS, no images or scripts; light and dark.

### Fixed

- Configurable option values are priced in every active currency: the
  admin forms show and accept `price_<CUR>_<cycle>` like product pricing
  (the older `price_<cycle>` fields still mean the default currency). A
  value charged in another currency but not priced in the cart currency is
  no longer free: pricing refuses it ("… is not available … in <CUR>") and
  the storefront does not offer it in that currency. A value no currency
  prices stays free.
- CMS pages: the copy pages of `/contact` and `/domains` can be saved again
  (and re-created after deletion), and a page may use a product group's
  slug, which makes it that group's landing page, as documented. Area
  routes (`admin`, `cart`, `checkout` …) stay reserved.

### Security

- The version is no longer exposed on the anonymous `/healthz` and
  `/api/v1/ping` endpoints. It is shown to the owner only.
- `HOS_TRUSTED_PROXIES` (addresses or CIDR ranges): the client address
  headers (`HOS_CLIENT_IP_HEADER`, `X-Forwarded-For`) and
  `X-Forwarded-Proto` count only on connections from these peers, so a
  visitor who reaches the port directly cannot choose their rate-limit
  address. Without it the headers are believed from every peer as before,
  and a warning is logged at startup.
- Public demo: a visitor who sends a request body slowly can no longer
  hold up the demo reset; the body is read in full before the request
  counts as running, and the reset waits at most 30 seconds for running
  requests.

[Unreleased]: https://github.com/sunucu-com-tr/hostingos/commits/main
