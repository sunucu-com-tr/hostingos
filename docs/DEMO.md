# Public demo mode

`HOS_DEMO_MODE=1` turns a HostingOS installation into a public demo:
anyone can log in with published accounts and try everything — browse the
storefront, order and pay with the mock gateway, manage the VPS that gets
set up, add products, clients, invoices, tickets, CMS pages, change the
storefront's look. Every 30 minutes (configurable) the demo is reset to its
sample data and **everything visitors entered is deleted**.

The code is in `crates/hos-web/src/demo.rs` (keep this document in sync
with it); how to deploy a demo server is in
[deploy/DEMO-DEPLOY.md](deploy/DEMO-DEPLOY.md)
([Türkçe](deploy/DEMO-DEPLOY.tr.md)).

Demo mode is **off by default** and independent of `HOS_DEBUG`; the
server refuses to start with both switched on. A demo is always a
single-company installation (`HOS_MODE=single`, the default): one hosting
company, created by the demo itself. `HOS_MODE=multi` together with demo
mode is refused.

## At a glance

| | |
|---|---|
| Switch | `HOS_DEMO_MODE=1` |
| Reset interval | `HOS_DEMO_RESET_MINUTES`, default **30** (5–1440) |
| Reset now | `hostingos demo-reset` (in the running container), or restart the server |
| Health check | `hostingos healthcheck` (plain HTTP `GET /healthz` on `HOS_BIND`, exit 0 / 1; `--path /readyz` also checks the database) |
| Hosts | One: the host of `HOS_BASE_URL` serves the storefront at `/`, the client area at `/client` and the admin panel at `/admin` (with the owner's server pages at `/admin/server`). No wildcard DNS, no custom-domain step. Any other host that reaches the app shows the demo too |
| Staff login | `admin@demo.test` / `demo-admin` (hosting company owner, `/admin`) |
| Client login | `musteri@demo.test` / `demo-client` (`/client`) |
| E-mail | Nothing is sent; the demo mailbox at `/demo/mailbox` shows what would have been |
| Integrations | Mock payment gateway, hypervisor, hosting panel and registrar only; the process makes no outbound connections |

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `HOS_DEMO_MODE` | off | `1` / `true` / `yes` / `on` switches demo mode on |
| `HOS_DEMO_RESET_MINUTES` | `30` | Minutes between two resets, 5–1440. Another value is ignored with a warning in the log |
| `HOS_DEMO_ALLOW_EXISTING_DATA` | off | Start even though the database holds other companies or operator accounts (see below). Only for a deliberately throw-away database |

Everything else is configured as for any installation: `DATABASE_URL`
(and `DATABASE_URL_DIRECT` if the pooled URL goes through PgBouncer),
`HOS_SECRET_KEY`, `HOS_BASE_URL`, `HOS_BIND`, and behind a reverse proxy
`HOS_TRUST_PROXY`, `HOS_TRUSTED_PROXIES`, `HOS_TRUSTED_PROXY_HOPS` and
`HOS_CLIENT_IP_HEADER` (see [Client IP behind proxies](#client-ip-behind-proxies)).
`HOS_MIGRATE_ON_START` is not needed: demo mode always migrates at startup.

Run exactly **one** `hostingos serve` process (web, job worker and reset
loop together). Rate limits and the reset gate live in that process'
memory; replicas or a separate `hostingos worker` would bypass them.

## Demo accounts

| Role | Login page | E-mail | Password |
|---|---|---|---|
| Hosting company owner (all admin permissions) | `/auth/login` → `/admin` | `admin@demo.test` | `demo-admin` |
| Client (Ayşe Yılmaz, with an unpaid VPS order: pay it with the mock gateway and the VPS is set up on the mock hypervisor) | `/auth/login` → `/client` | `musteri@demo.test` | `demo-client` |

The login pages list these accounts with a button that fills in the form.
The passwords are public on purpose; they are the same after every reset
and cannot be changed.

## What visitors see

A banner on every page, in the visitor's language, with the configured
interval (here 30) and a live countdown:

* English: "This is a demo. Content here may have been entered by other
  visitors as test data, and you can add your own: everything entered is
  kept for at most 30 minutes and then deleted automatically. Do not enter
  real personal data. Next reset in about 12 min."
* Turkish: "Bu bir demodur. Buradaki içeriğin bir kısmını diğer
  ziyaretçiler deneme amacıyla girmiş olabilir; siz de kendi içeriğinizi
  ekleyebilirsiniz. Girilen her şey en fazla 30 dakika saklanır, ardından
  otomatik olarak silinir. Gerçek kişisel bilgi girmeyin. Sonraki
  sıfırlamaya yaklaşık 12 dakika var."

The countdown is rendered by the server and updated every 15 seconds by
`static/js/app.js` (`data-demo-reset-at`, no inline script). When the time
is up it reads "The demo is being reset now; reload the page in a moment."

The login page adds the demo accounts and: "This is
a demo: sign in with one click and add your own test content. Everything
you enter is kept for at most 30 minutes and then deleted automatically.
The demo passwords cannot be changed." (Turkish: "Bu bir demodur: tek
tıkla giriş yapın ve kendi deneme içeriğinizi ekleyin. Girdiğiniz her şey
en fazla 30 dakika saklanır, ardından otomatik olarak silinir. Demo
parolaları değiştirilemez.")

The texts are in `locales/{en,tr}/demo.json`; the number always follows
`HOS_DEMO_RESET_MINUTES`.

## Safety at startup

1. **No debug mode.** `HOS_DEBUG=1` together with demo mode is refused
   (debug mode shows internal error details).
2. **Database check.** The server refuses to start in demo mode when the
   database contains any company other than the demo company (`demo`), or
   any operator account of multi-company mode. Demo mode deletes and
   re-creates the demo company, so it must never run against a production
   database or a shared development database. `HOS_DEMO_ALLOW_EXISTING_DATA=1` overrides the check
   (it is logged).
3. **Migrations.** Pending migrations are applied before the listener
   opens, as with `hostingos serve --migrate` (the demo database is the
   demo's own), so the server also starts on an empty database and no
   request meets a half-migrated one. A failed migration stops the start.
   A `hostingos migrate` run at the same time (for example a deployment
   tool's post-deployment command) waits for the same database lock and
   then finds nothing to do, so it is harmless but no longer needed.
4. **Offline.** The process switches its outbound guard
   (`hos_core::net::go_offline`) into a state that refuses every outbound
   connection made on behalf of the company or a visitor, before any DNS lookup:
   integrations, webhooks, OAuth, IndexNow, RDAP / WHOIS, AI, custom-domain
   DNS checks. It cannot be switched back on without a restart.
5. **Mock integrations only.** The integration registry accepts only the
   `mock` providers (payment gateway, hypervisor, hosting panel, registrar,
   e-invoice) and the log mailer; a real provider's configuration is refused
   when it is loaded. An `ANTHROPIC_API_KEY` in the environment is ignored.
6. **No update checks.** The release source is disabled, whatever
   `HOS_UPDATE_REPO` / `HOS_UPDATE_MANIFEST_URL` say.
7. **First reset.** The demo is reset once before the server accepts
   requests, so every process starts from a clean, known state.
8. **One company.** `HOS_MODE=multi` is refused: the demo company is the
   installation's only company, so there is no operator console and no
   operator account; the owner's server pages are read-only.

## The reset

Every `HOS_DEMO_RESET_MINUTES` (default 30) the reset loop in the server:

1. holds new requests back until it is done and waits until requests in
   flight have finished, at most 30 seconds; after that it resets anyway
   (logged as `demo reset: requests were still running; resetting
   anyway`), so a slow or stuck request cannot postpone the reset. A
   request counts as in flight only once its body has arrived in full: a
   visitor who sends a body slowly holds up nothing (the body itself is cut
   off after `HOS_HTTP_BODY_TIMEOUT_SECS`, docs/DEPLOYMENT.md);
2. in one database transaction, under a PostgreSQL advisory lock (so two
   processes can never reset at the same time):
   * deletes the demo company (`demo`), which removes everything visitors created
     (products, clients, orders, invoices, tickets, pages, uploads, settings,
     e-mails, sessions …) through the database's cascading deletes;
   * seeds it again with the same id: catalogue and prices, TLDs,
     currencies, tax rules, roles, support departments, site pages,
     knowledge base, announcements, blog posts, e-mail templates and
     settings;
   * makes the host of `HOS_BASE_URL` the company's primary, verified domain
     (unless it is `localhost`; then the local names `demo.localhost` etc.);
   * creates the two logins with their documented passwords and the demo
     client account;
   * configures the mock hypervisor, panel, registrar and payment gateway
     and links them to the seeded products;
3. after the commit: the scheduled tasks of the company and one unpaid VPS
   order with its invoice for the demo client (so "pay an invoice" works
   right away);
4. records a new *generation* (a timestamp, kept in the company's settings),
   empties the in-process caches (company, theme, storefront, integrations)
   and clears the lockout counters of the demo accounts.

A reset takes well under a second on a small server. If one fails it is
logged and retried a minute later; the previous state stays usable
meanwhile.

**Manual reset.** `hostingos demo-reset` runs the same reset from a second
process (for example `docker compose exec app /usr/local/bin/hostingos
demo-reset`, or Coolify's Terminal / a Scheduled Task with "Execute Now").
It needs the same environment as the server (`HOS_DEMO_MODE=1`, database,
secret key) and refuses a database with other data, like the server. The
running server notices the new generation within 30 seconds, empties its
caches and starts a new interval. Restarting the server resets too.

**Visitors caught by a reset.** The browser keeps the generation it last
saw in a cookie (`hos_demo_gen`, no personal data). When it no longer
matches:

* a form post is **not** processed (its session, CSRF token and ids belong
  to deleted data): the visitor goes back to the page with the message
  "The demo was just reset: everything entered in the last 30 minutes was
  deleted and the sample data was restored. Please sign in again to
  continue." (Turkish: "Demo az önce sıfırlandı: son 30 dakikada girilen her
  şey silindi ve örnek veriler geri yüklendi. Devam etmek için lütfen
  yeniden giriş yapın."); htmx requests get an `HX-Redirect` to that page;
* a page view shows the same message (on the login page if the page needs
  a session, which the reset deleted).

No CSRF error, no `500`, no broken session.

## What visitors can do

Practically everything a hosting company and its clients do, with data
that lasts until the next reset:

* Storefront: browse in Turkish or English, light or dark mode, search
  domains (mock registrar), add to cart, check out, pay with the mock
  gateway, accept or decline quotes, open guest tickets.
* Client area: services (power, reinstall, snapshots, console of mock VMs),
  domains (name servers, contacts, DNS), invoices, credit, tickets with
  image attachments, profile details, sub-users and API keys of the client
  account.
* Admin panel: clients, orders, invoices, services, domains, quotes, tasks,
  affiliates, tickets, reports, search, coupons, currencies, tax; the
  catalogue (products, groups, bundles, add-ons, options, TLDs, prices);
  CMS pages, landing pages, blog, knowledge base, announcements, downloads
  (images), network status, SEO meta and redirects, fraud rules; staff
  invitations and roles; API keys; company details, logos (raster images),
  order, billing, language, automation and e-mail sender settings;
  appearance (template, colours, fonts); e-mail template previews and test
  mails; "Test connection" of the mock integrations.
* Server pages (`/admin/server`: version and health, update management):
  visible, nothing can be changed.
* **Demo mailbox** (`/demo/mailbox`, linked in the banner when logged in):
  every e-mail the demo "sent" (invitations, invoices, ticket replies …).
  Nothing leaves the server. Staff see all e-mails of the demo company, the
  client only its own. Recipient addresses other than the demo accounts
  are shown masked (`j***@e***.org`); the mailbox is emptied by every reset.

What visitors enter goes through the same validation and sanitising as on
any installation: Markdown is rendered with raw HTML shown as text and
`javascript:` / `data:` links neutralised; there are no fields for raw
HTML, scripts or custom code (the HTML e-mail templates cannot be edited
in the demo); uploads are type-checked from their content.

## What is blocked

Only what reaches other hosts, could lock other visitors out, or would let
visitors run code. A blocked request gets a "This action is switched off
in the public demo" message (form posts return to their page with that
message; API calls get a `403` problem+json with code `demo_disabled`). The
policy is a pure function (`demo::verdict`) with unit tests.

**Always blocked (any method)**

| What | Paths |
|---|---|
| Public sign-up, password reset, OAuth sign-in (the logins are shared) | `/auth/register`, `/auth/forgot`, `/auth/reset`, `/auth/oauth/**` |
| WHOIS lookups (outbound) | `/whois` |
| Inbound e-mail webhooks (ticket piping from other hosts) | `/webhooks/mail/**` |
| WHMCS import | `/admin/tools/import/**` |
| Internal style guide | `/admin/_styleguide` |

Pages of multi-company operation (the operator console under `/platform`,
custom domains under `/admin/settings/domains`) do not exist in
single-company mode at all and answer `404`.

**Blocked state changes (POST etc.; reading stays allowed)**

| What | Paths |
|---|---|
| The server pages: update checks and installs, update settings, maintenance mode | `/admin/server/**` |
| The REST API (read-only: keys are bearer credentials for scripts) | `/api/**` |
| The shared demo logins: 2FA, password, linked identities, sessions, account deletion | `/auth/2fa/setup`, `/auth/2fa/disable`, `/client/account/password`, `/client/account/identities/**`, `/client/account/sessions/**`, `/client/account/privacy/delete` |
| Deactivating, demoting or resetting the demo admin; closing the demo client | `/admin/staff/{demo admin}/…`, `/admin/clients/{demo client}/status` |
| Policies that would lock every visitor out (mandatory staff 2FA, session lifetimes) or contradict the closed sign-up | `/admin/settings/security`, `/admin/settings/registration` |
| Raw HTML: e-mail templates (preview and test mail stay allowed) | `/admin/settings/email/templates/**` |
| Real integrations and anything that reaches other hosts: creating / editing / deleting integrations (the mocks are preconfigured), outgoing webhooks, e-invoice provider settings, inbound mail, AI, IndexNow | `/admin/integrations`, `/admin/integrations/*`, `/admin/integrations/*/delete`, `/admin/developer/webhooks/**`, `/admin/settings/einvoice`, `/admin/support/inbound/**`, `/admin/support/tickets/*/ai-draft`, `/admin/ai/**`, `/admin/seo/indexnow/**` |

**Other restrictions**

* The demo logins are never locked by wrong passwords (anyone could lock
  every visitor out); the per-IP login rate limit still applies.
* Uploads (ticket attachments, downloads): raster images only (PNG, JPEG,
  GIF, WebP), type checked from the file content; ticket attachments at
  most 3 files of 600 KB per message. Logos and favicons: raster images
  with the usual dimension checks. SVG, HTML, PDF, text and archives are
  refused. Attachments and downloads are always served as downloads with
  `Content-Security-Policy: sandbox` and `nosniff`.
* Links in Markdown content get `rel="nofollow ugc noopener"`. Ticket
  messages are shown without links or images (a link is shown as text).
* Public sign-up for clients is off (`auth.allow_registration`); guest
  tickets through the contact form stay possible.
* Visitor IP addresses are stored truncated (IPv4 `/24`, IPv6 `/48`) in
  sessions, login history and the audit log, because visitors share
  accounts and would otherwise see each other's addresses.
* Search engines: every response carries
  `X-Robots-Tag: noindex, nofollow, noarchive`; `robots.txt` disallows
  everything and lists no sitemap. IndexNow pings are off.

**Rate limits per client IP** (in addition to the normal limits):

| What | Limit |
|---|---|
| All requests (assets excluded) | 240 per minute |
| State-changing requests | 60 per 10 minutes |
| Expensive pages: PDFs, print views, CSV / exports, reports, search, generated images, domain checks | 20 per 5 minutes |

## Client IP behind proxies

The rate limits are only per visitor if the app sees the visitor's
address. `HOS_TRUST_PROXY=true` makes it read the address from the
request, but only when the connection comes from an address in
`HOS_TRUSTED_PROXIES` (the reverse proxy; an empty list believes every
connection and logs a warning at startup). Which header it uses:

1. `HOS_CLIENT_IP_HEADER` (for example `CF-Connecting-IP`), when set and
   the request carries exactly one valid address in it;
2. otherwise the `X-Forwarded-For` entry `HOS_TRUSTED_PROXY_HOPS` places
   from the right (the leftmost entries are client-controlled);
3. otherwise the TCP peer.

A connection from any other address is counted under its own address,
whatever headers it sends. Malformed values (names, several addresses in
the client-IP header, IPv6 zone ids) are skipped, never trusted.

**Cloudflare → Coolify's Traefik → app** (the public demo): Coolify
starts Traefik without `forwardedHeaders.trustedIPs` / `insecure`, so
Traefik does not trust the `X-Forwarded-For` Cloudflare sends and replaces
it with Cloudflare's own address. No `HOS_TRUSTED_PROXY_HOPS` value can
find the visitor there (with `1` every visitor would share one Cloudflare
address per edge server). Cloudflare also sends `CF-Connecting-IP`, which
Traefik passes on unchanged, so set:

```env
HOS_TRUST_PROXY=true
HOS_CLIENT_IP_HEADER=CF-Connecting-IP
HOS_TRUSTED_PROXY_HOPS=1    # fallback only: Traefik's single entry
HOS_TRUSTED_PROXIES=…       # the Docker network Traefik reaches the app through
```

docs/deploy/DEMO-DEPLOY.md shows how to find that network's range.

If Traefik is configured to trust Cloudflare's address ranges
(`forwardedHeaders.trustedIPs`), `X-Forwarded-For` arrives as
`visitor, cloudflare` and `HOS_TRUSTED_PROXY_HOPS=2` would also work; the
header setting is right in both cases.

The header can be forged by anyone who reaches the server without going
through Cloudflare: such a request still arrives through Traefik, so
`HOS_TRUSTED_PROXIES` does not catch it. Allow ports 80 / 443 only from
[Cloudflare's address ranges](https://www.cloudflare.com/ips/) (or use a
Cloudflare Tunnel), otherwise a visitor can choose their own rate-limit
bucket.

The scheme needs no setting: with an `https://` `HOS_BASE_URL` the app
treats every request as https (Secure cookies, CSRF origin check), even
when Traefik reports the plain-HTTP hop from Cloudflare.

## Hostile visitor review

A demo visitor is a logged-in owner of the hosting company who may be hostile. What that
could be used for, and what bounds it:

| Attack | Bound |
|---|---|
| **SSRF / reaching other hosts** (integration URLs, webhooks, mail servers, AI endpoints, RDAP, DNS checks, OAuth) | The process is offline (every guarded client refuses, before DNS); only mock providers can be built; integrations and webhooks cannot be created, custom domains do not exist; update checks are disabled. Tests: `demo_refuses_outbound_traffic_raw_html_and_lockouts`, `demo_registry_uses_only_mocks_and_the_log_mailer` |
| **Running code in other visitors' browsers** | No raw HTML fields (e-mail templates are blocked); Markdown shows raw HTML as text; strict CSP without inline scripts; uploads are raster images, checked by content, served as downloads; SVG is refused everywhere |
| **Reading server files** | Templates and migrations are compiled in; static files are served from one directory with path normalisation; uploads are stored in the database; there is no file import (WHMCS import is blocked) |
| **Leaking secrets / environment** | The server pages show only the version, uptime, database latency, migrations, the public address and on/off flags, never configuration values or secrets; nothing there can be changed; debug mode cannot be combined with demo mode; error pages show no internals; integration secrets are encrypted and never shown |
| **Resource exhaustion** | Per-IP limits above; 2 MiB request bodies; upload limits; slow clients are cut off (request headers 120 s, body 60 s, whole request 120 s: `HOS_HTTP_*_TIMEOUT_SECS`); PDFs, exports, reports and search are in the "expensive" limit; all data is deleted at the next reset; container CPU / memory limits in `docker-compose.demo.yml` |
| **PDF generation** | Pure Rust (no external renderer, no network, no file access); fonts are compiled in; rate-limited |
| **Exports** | CSV cells that start with `=`, `+`, `-`, `@` are prefixed so spreadsheets do not run them as formulas; rate-limited |
| **Search** | Parameterised queries scoped to the company; rate-limited |
| **Taking over the demo** | The demo logins' password, e-mail, 2FA and sessions cannot be changed; the demo admin cannot be deactivated or demoted, the demo client cannot be closed; security policies cannot be changed; no lockout; the server pages are read-only; everything is restored at the next reset |
| **Other visitors' personal data** | Shared accounts, so visitors see what others typed during the same interval; the banner says so; IP addresses are truncated; e-mail addresses are masked in the mailbox; everything is deleted at the next reset |

## Residual risks (accepted)

* **Visitor content is public until the next reset.** One visitor's
  products, pages, announcements, images, client names or ticket texts —
  possibly offensive or misleading — are visible to other visitors, and
  pages on the storefront to anyone, for up to one reset interval. What
  keeps it bounded: the banner on every page says content may be
  visitor-entered test data; every response is `noindex` and `robots.txt`
  disallows crawling; links in content are `nofollow ugc`; uploads are
  raster images only; the interval is short (30 minutes); and the operator
  can delete everything at once with `hostingos demo-reset` (or a restart).
* **Visitors see each other's data.** Everyone logs in as the same staff
  member and the same client. The banner and login pages say not to enter
  real personal data.
* **Phishing-looking content.** A visitor could publish a page that
  imitates a login form or another company. It lives on the demo's host,
  carries the demo banner, cannot run scripts, and is deleted at the next
  reset; report-and-reset is the operator's remedy.
* **Many IP addresses.** The per-IP limits do not stop a distributed
  attack. Keep the demo behind a proxy or CDN with its own rate limiting if
  that matters, and keep the container's CPU and memory limits.
* **Forged client-IP header.** `HOS_CLIENT_IP_HEADER` is only as
  trustworthy as the path to the server; see
  [Client IP behind proxies](#client-ip-behind-proxies).
* **In-memory limits.** Rate limits and lockout exemptions are per
  process and start empty after a restart.
* **Database growth within an interval** is bounded by the write limits
  per IP, not overall; the reset deletes everything, and the demo database
  has no other data to protect.
* **Mock gateway.** Any visitor can mark any demo invoice as paid; that is
  the point of the demo.
* **Shared database instance.** Demo mode protects other data only by
  refusing to start next to other companies. `HOS_DEMO_ALLOW_EXISTING_DATA=1` removes
  that protection; never set it on a database whose data matters.

## Tests

Unit tests in `crates/hos-web/src/demo.rs` cover the request policy, the
protected demo logins, the expensive-page list, IP and address masking and
the upload types and the reset gate
(`the_reset_gate_waits_bounded_and_holds_new_requests_back`);
`markdown.rs` covers link-free user text; `config.rs`, `extract.rs` and
`tests/it/security.rs` the client-IP header and `HOS_TRUSTED_PROXIES`
(`forged_client_ip_header_from_an_unlisted_peer_is_ignored`); `server.rs`
the HTTP timeouts; `tests/it/schema.rs` startup migrations and the
behind / ahead schema check; `cli.rs` the health check.
The integration tests in `crates/hos-web/tests/it/demo.rs` run against
isolated throw-away companies (never the database's real `demo` company):

| Test | Checks |
|---|---|
| `demo_reset_restores_the_seed_and_keeps_logins_working` | A reset deletes visitor data, keeps the company id, restores the accounts and their passwords; logins work afterwards |
| `demo_refuses_outbound_traffic_raw_html_and_lockouts` | Imports, integrations, webhooks, security and registration settings, e-mail templates, AI, IndexNow and sign-up are refused; custom domains answer `404`; the demo admin cannot be deactivated; refusals explain themselves |
| `demo_visitors_can_publish_test_content_until_the_reset` | Catalogue, content, staff, settings and other admin writes are allowed; a published CMS page appears on the storefront, sanitised, and is gone after the reset |
| `demo_reset_mid_form_explains_itself` | A form sent across a reset is not processed, the visitor gets the notice and a fresh login; htmx gets `HX-Redirect` |
| `demo_server_follows_a_reset_run_elsewhere` | A reset by `hostingos demo-reset` is picked up by the server |
| `demo_reset_is_not_held_up_by_a_slow_request_body` | A request whose body is still arriving does not count as in flight; the reset runs while it waits |
| `demo_serves_storefront_and_admin_on_the_base_url_host` | Single host: the `HOS_BASE_URL` host is the primary domain; storefront, admin and server pages on it, `/platform` answers `404`; https origin behind a plain-HTTP proxy hop; Secure cookies without `Domain` |
| `demo_answers_on_unknown_hosts` | Any host shows the demo, never "site not found" |
| `demo_logins_cannot_be_changed_or_locked` | No lockout, no password / 2FA / session / account deletion, the demo client cannot be closed, stored IPs are truncated |
| `demo_server_pages_are_read_only` | No `/platform`, no third demo account; `/admin/server` pages load with the banner, every change there is refused |
| `demo_pages_carry_banner_noindex_and_login_hints` | Banner in both languages with the countdown, `X-Robots-Tag`, `robots.txt`, demo logins on the login pages |
| `demo_mailbox_shows_captured_mail_with_masked_addresses` | Captured mail, masking, client sees only its own mail |
| `demo_registry_uses_only_mocks_and_the_log_mailer` | Real providers are refused, the log mailer is used |
| `demo_mode_refuses_a_database_with_other_data` | The startup check and its override |
