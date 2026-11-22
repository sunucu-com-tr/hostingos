# hos-web — HostingOS web layer

The axum application (binary `hostingos`). It owns the HTTP pipeline, the
design system (Askama templates + `static/`), i18n (`locales/`), the SEO-first
storefront, the auth flows and the skeletons of the client area, admin panel,
platform console, REST API and webhooks. `docs/ARCHITECTURE.md` (§3, §6, §7)
is binding. This file covers the conventions every area follows. Keep them
consistent, and when a convention changes, update this file in the same change.

```text
crates/hos-web/
  src/
    main.rs, cli.rs        clap: serve | worker | migrate | create-platform-admin | create-staff | seed-demo | tenant | jobs
    worker.rs              background runtime (hos-jobs Worker + Scheduler, maintenance), shared shutdown
    state.rs               AppState (pool, config, secret box, CSRF, limiters, lockout, i18n, assets, caches, mailer)
    tenant.rs, locale.rs   Host → tenant, /en prefix handling
    session.rs, csrf.rs, flash.rs, middleware.rs
    extract.rs             request extractors (see below)
    view.rs                PageCtx (template context + helpers), menus, Pager, render()
    error.rs               AppError → HTML error page or application/problem+json
    i18n.rs, forms.rs, seo.rs, mail.rs, markdown.rs, assets.rs
    routes/
      mod.rs               app(): merges the area routers, middleware stack
      guards.rs            require_client / require_staff / require_platform_admin
      storefront/ auth/ client/ admin/ platform/ api/ webhooks/ health.rs
  templates/
    layouts/  base.html → storefront.html | auth.html | panel.html
    components/ui.html (macros), catalog.html, site_header.html, site_footer.html
    <area>/*.html, errors/error.html
  tests/                   oneshot integration tests (real DB, throw-away tenant)
  examples/seo_audit.rs    hos-seo audit + GEO report for a saved page
locales/{tr,en}/<area>.json   static/{css,js,img,vendor}
```

## Running

```sh
. "$HOME/.cargo/env"
cargo run -p hos-web -- migrate
cargo run -p hos-web -- seed-demo                  # demo tenant + 3 accounts (passwords printed once)
cargo run -p hos-web -- seed-demo --reset-passwords
cargo run -p hos-web -- serve                      # HOS_BIND (default 127.0.0.1:8080)
cargo run -p hos-web -- create-platform-admin --email ops@example.com   # password: HOS_NEW_PASSWORD or random
cargo run -p hos-web -- create-staff --tenant demo --email a@b.c --role owner
cargo run -p hos-web -- serve --no-worker          # web only; background runtime elsewhere
cargo run -p hos-web -- worker                     # job worker + cron scheduler + platform maintenance
cargo run -p hos-web -- tenant create --slug acme --name "Acme" --owner-email o@acme.test [--domain www.acme.test]
cargo run -p hos-web -- tenant list [--search acme]
cargo run -p hos-web -- jobs run-once [--max-jobs 1000]
```

`serve` runs the `hos-jobs` `Worker` + `Scheduler` and the platform
maintenance loop (custom-domain DNS re-checks, expired idempotency keys) in
the same process (`src/worker.rs`); SIGTERM/SIGINT stops the HTTP server and
the background tasks together (unfinished jobs are handed back). The worker
`LISTEN`s on `DATABASE_URL_DIRECT`; `HOS_WORKER_CONCURRENCY` (default 8).
`mail::send_email` only queues: it renders nothing itself but calls
`Billing::queue_email` (tenant mailer from the integration registry, logged
when none is configured), so handlers never block on SMTP.

In debug mode (`HOS_DEBUG`, on in debug builds) `localhost` serves the
`HOS_DEV_TENANT` slug (default `demo`); in production a bare loopback host is
no tenant. Debug mode also allows the `mock` integrations (logged as a
warning at start-up); outside debug mode they need
`HOS_ALLOW_MOCK_INTEGRATIONS=1` (`WebConfig::mock_integrations`). Start-up
(`AppState::from_env`) fixes the process-wide `HOS_OUTBOUND_POLICY` and refuses
to start on an invalid value. Other hosts resolve through `tenant_domains` (only verified custom
domains), with `<slug>.<HOS_BASE_DOMAIN>` as the fallback; the label of the
console's own host (`HOS_BASE_URL` = `https://<label>.<HOS_BASE_DOMAIN>`) is
never a tenant slug and cannot be registered. A `Host` that is not strictly
`hostname[:port]` (or `[ipv6][:port]`) is answered with `400`; the request
origin (`PageCtx.origin`: e-mail links, canonical, sitemap, llms.txt) is
built from that validated host only. Web-only variables: `HOS_STATIC_DIR`,
`HOS_LOCALES_DIR`, `HOS_DEBUG` (on in debug builds), `HOS_TRUST_PROXY`,
`HOS_TRUSTED_PROXIES`, `HOS_TRUSTED_PROXY_HOPS`, `HOS_CLIENT_IP_HEADER`,
`HOS_MAX_BODY_BYTES`, `HOS_HTTP_HEADER_TIMEOUT_SECS` / `_BODY_` / `_REQUEST_`
(120 / 60 / 120 s), `HOS_MIGRATE_ON_START`, `HOS_LOG_FORMAT=json`, `RUST_LOG`.

Behind a reverse proxy set `HOS_TRUST_PROXY=1` and `HOS_TRUSTED_PROXY_HOPS` to
the number of proxies that append to `X-Forwarded-For` (default `1`: the
client IP is the rightmost entry, the one your proxy wrote; `2` for e.g. CDN
→ load balancer; `0` ignores the header and uses the socket address). The
leftmost entries are client-controlled and are never used on their own.
`HOS_TRUSTED_PROXIES` (addresses / CIDR ranges) limits all forwarding
headers to connections from those peers (`WebConfig::forwarded_headers_trusted`);
`extract::ClientIp` is the only place that reads the client address.

## Request pipeline

```text
CatchPanic → SetRequestId → Trace → PropagateRequestId → security headers (CSP, HSTS on https…)
  → request limits (body read in full: size + timeout; request timeout; crate::server)
  → demo layer → compression → body limit → schema check (503 while migrations are pending)
  → maintenance
  ├─ /static/*, /assets/app.css, /healthz, /readyz           (no tenant, no session)
  └─ resolve_tenant → locale_prefix → load_session → error_pages → csrf_protect
       → flash → cache_control → CatchPanic → area routers
```

* **CSP** is `script-src 'self'; style-src 'self'`. Do not use inline
  `<script>`, `style="…"` or `on*=` handlers. Express behaviour with
  `data-*` attributes wired in `static/js/app.js` (`data-dialog-open`,
  `data-copy`, `form[data-confirm]`, `data-autosubmit`, `data-theme-toggle`,
  `data-sidebar-toggle`). JSON-LD `<script type="application/ld+json">` is fine.
* `/platform/*` is tenantless. Everything else 404s on an unknown host and
  returns a 503 info page for a suspended tenant.
* CSS is one bundle, `/assets/app.css?v=<hash>` (cascade layers: `tokens`,
  `base`, `layout`, `components`, `utilities`, `pages`). Add rules to the
  matching file in `static/css/`. A later layer always wins regardless of
  specificity, so an override of a component inside a layout region
  (`.site-header__actions .btn`) belongs in `components.css`, not
  `layout.css`. Use the tokens in `tokens.css`, never raw
  colours, so dark mode keeps working.

## Extractors (`crate::extract`, `crate::tenant`, `crate::view`)

| Extractor | Gives | Rejects with |
|---|---|---|
| `State<AppState>` | pool, config, i18n, limiters … | — |
| `Tenant` | `TenantContext` (id, slug, branding, currency, locales) | 404 |
| `PageCtx` | template context: locale, tenant, user, csrf, flashes, title/SEO, helpers | — |
| `MaybeUser` | `Option<Arc<SessionInfo>>` | — |
| `CurrentUser` | any logged-in tenant user | 401 (browser: redirect to login) |
| `CurrentClient` | `user`, `client_id`, … | 401 / 403 |
| `CurrentStaff` | staff + `PermissionSet`; `.can(p)`, `.require(p)?` | 401 / 403 |
| `RequirePermission<perm::BillingManage>` | `.staff` | 401 / 403 |
| `PlatformAdmin` | platform operator session | 401 / 403 |
| `ClientIp` | IP (forwarding headers only with `HOS_TRUST_PROXY=1` and a peer in `HOS_TRUSTED_PROXIES`: `HOS_CLIENT_IP_HEADER`, else `HOS_TRUSTED_PROXY_HOPS` entries from the right); `.key("action")` for limiters | — |
| `RequestHost` | the validated `Host` (`name`, `port`, `.authority()`) | 404 when absent |
| `UserAgent`, `Htmx` (`.request`, `.wants_fragment()`), `PageQuery` | | — |
| `api::ApiAuth` | API key (Bearer) or session principal | 401 / 403 problem+json |

## Adding a page

1. **Handler.** Add it in the area module (`routes/client/services.rs`, …) and
   register it in that area's `router()`. Area guards are already applied with
   `route_layer`, so a client route needs nothing extra. Admin routes still
   check their permission.
2. **Template struct.** It has a `ctx: PageCtx` field and
   `crate::page!(MyPage);`, which derefs to `PageCtx`, so templates can call
   `t()`, `url()`, `money()` … directly.
3. **Template.** Create `templates/<area>/<name>.html`, extend a layout and
   fill `{% block content %}`.
4. **Translations.** Add the keys to `locales/tr/<area>.json` and
   `locales/en/<area>.json`. A unit test fails when any key used in
   `src/` or `templates/` is missing from either catalog.
5. **Menu.** Add an entry to `routes/<area>/menu.rs`. For admin, the
   `permission` field only hides the entry. The handler must still check.

```rust
#[derive(Template)]
#[template(path = "client/services.html")]
struct ServicesPage { ctx: PageCtx, rows: Vec<ServiceRow>, pager: Pager }
crate::page!(ServicesPage);

async fn services(State(state): State<AppState>, tenant: Tenant, client: CurrentClient,
                  q: PageQuery, mut ctx: PageCtx) -> AppResult<Response> {
    let paged = services::list_for_client(&state.pool, tenant.id, client.client_id, q.db_page()).await?;
    let title = ctx.t("client.services.title");
    ctx.set_title(title.clone());
    ctx.panel = Some(menu::chrome(&ctx, vec![Crumb::current(title)]));   // admin: menu::chrome(&ctx, &staff, crumbs)
    let pager = Pager::new(&paged, &ctx.url("/client/services"), &[("q", q.q.as_deref().unwrap_or(""))]);
    render(&ServicesPage { rows: paged.items.iter().map(ServiceRow::from).collect(), pager, ctx })
}
```

```html
{% extends "layouts/panel.html" %}
{% import "components/ui.html" as ui %}
{% block content %}
<div class="page-head"><div><h1>{{ ctx.title }}</h1></div></div>
{% call ui::card() %}…{% endcall %}
{% endblock %}
```

**Layouts.**
* `layouts/storefront.html`: site header/footer, `{% block content %}`.
  Storefront pages call `seo::apply(&mut ctx, SeoPage::new(title, path)…)`,
  which sets the title, description, canonical, hreflang, OG and JSON-LD.
* `layouts/auth.html`: centered card, `{% block card %}`.
* `layouts/panel.html`: sidebar + topbar + breadcrumbs from `ctx.panel`
  (`PanelChrome`), with `{% block topbar %}` and `{% block content %}`.

**Askama 0.16 gotchas.**
* `{% call %}` always needs `{% endcall %}`.
* Pass `vec.as_slice()` to macros, not the field itself.
* A bare `None` macro argument cannot be inferred, so pass a typed field.
* There are no custom filters: add a helper method on `PageCtx` instead.
* Helpers that receive `~` concatenations or `t(…)` results take `impl Display`.

## Template helpers and components

Helpers on `PageCtx`, all callable from templates:

* Translation: `t(key)`, `tv(key, "name", v)`, `tv2(…)`, `tn(key, n)`.
* URLs: `url("/kb")` gives the locale-prefixed path; also `url_in`,
  `abs(path)`, `switch_url("en")`, `current_url()`, `is_current("/client")`.
* Formatting: `money(&d)`, `money_in(&d, &cur)`, `num(n)`, `date(&dt)`,
  `datetime(&dt)`, `iso(&dt)`, `cycle(&c)`, `per(&c)`.
* User and permissions: `logged_in()`, `user_name()`, `home_url()`,
  `can("billing.view")`, `debug()`.
* Atoms: `icon("server")`, `icon_cls("x", "icon--lg")`,
  `badge(text, "success")`, `status_badge("active")`. Icons are symbols in
  `static/img/icons.svg`. Status labels come from `common.status.<status>`.

Macros in `components/ui.html` (`{% import "components/ui.html" as ui %}`):

* Forms: `csrf()`, `field(name, label, value, kind=, error=, hint=,
  required=, autocomplete=, placeholder=, inputmode=, autofocus=)`, `textarea`,
  `select`, `checkbox`, `honeypot()`, `form_errors(errors)`.
* Feedback: `alert(kind, text)`.
* Containers and content (body via `caller()`): `card(title, subtitle)`,
  `empty(icon, title, text)`, `stat(label, value, hint, tone, icon)`,
  `breadcrumbs(crumbs)`, `pagination(pager)`, `steps(labels, current)`,
  `tabs(items, active)`, `modal(id, title, drawer=)`, `code(id, label, text)`,
  `skeleton(lines)`.
* Controls: `theme_toggle()`, `lang_switch()`, `logout(action, cls)`.

`components/catalog.html` holds the storefront pieces (`price_card`,
`cycle_switch`, `domain_search`). Every component is shown in
`/admin/_styleguide` (debug builds only).

### Design system

* Colour: warm ink on paper (light), neutral greys (dark). The primary
  button is ink; the tenant colour (`--brand` from `/brand.css`) is only an
  accent: links (`--brand-strong`, clamped in OKLCH so any brand keeps AA on
  paper), selection, focus ring, featured outline, badges. Text on a brand
  fill uses `--brand-ink`. No shadows on in-flow elements, 1px `--line`
  rules instead; `--shadow-lg` is for overlays only.
* Type: Geist / Geist Mono (OFL, `static/fonts`, `?v=1.9.2`). The URLs in
  `tokens.css` `@font-face` and the `<link rel=preload>` in
  `layouts/base.html` must stay identical. Mono (`.mono`, tabular) is for
  prices, domains, IPs, IDs and the small uppercase `.label` / `.eyebrow`.
  Size with `rem`/`em`, not `ch` (the fallback face has a different "0").
  Text and headings use `--font-sans` / `--font-heading`, which a tenant
  can change (see [Theming](#theming)); never name a family directly.
* Layout: `.section__head` (hairline + numbered mono eyebrow, optional
  `.section__aside`), `.cell-grid` (`--2/3/4`, `--rows-3/4` subgrid rows),
  `.spec-panel`, `.grid-12`. Cards in a row share rows through subgrid.
* No layout shift: state changes use `outline`, inset `box-shadow`,
  `visibility` or colour, never `border-width`, `display` or `transform` on
  in-flow elements; swapped fragments keep a fixed `min-height`; icons carry
  `width`/`height`; `.price-card__prices` stacks every billing cycle in one
  grid cell and the cycle switch only toggles visibility. Tables get
  `data-fits` (app.js) and only then a sticky header.

## Theming

Per-tenant appearance, edited at `/admin/settings/appearance`
(`settings.manage`). Code: `crate::theme` (`mod.rs`, `fonts.rs`, `color.rs`,
`presets.rs`) and `routes/admin/settings/appearance.rs`.

* Storage (no migration): `tenant_settings` keys `appearance.template`,
  `appearance.font_body`, `appearance.font_heading`, `appearance.radius`,
  `appearance.mode` (`""` = the template's default) and
  `appearance.mode_toggle` (bool). Brand / accent colours stay in
  `tenants.branding` (mail uses them too). A save audits
  `settings.appearance` / `settings.branding` and calls `theme::invalidate`.
* Resolution: `tenant::resolve_tenant` → `theme::attach` stores the
  tenant's `Arc<Theme>` (cached per tenant, 60 s TTL, branding colours in the
  key) in the request; it is `PageCtx.theme`. Template helpers:
  `customer_surface()`, `tpl()`, `theme_css_url()`, `template_css_url()`,
  `font_preloads()`, `color_mode()`, `mode_toggle()`, `theme_preview()`.
* Customer surfaces (storefront, cart, auth, client area; not `/admin` or
  `/platform`) get `<html data-template>`, `data-theme` when the tenant fixes
  a default mode and `data-mode-locked` when visitors may not toggle
  (`theme.js` then ignores the stored preference). Admin only picks up
  `/brand.css` (fonts, brand as accent): no template CSS, no radius change.
* `GET /brand.css?v=<12 hex>`: `@font-face` for the selected body / heading
  families only, with metric-matched local fallback faces; `:root` variables
  (`--brand`, `--brand-ink`, `--brand-hover`, `--brand-2`, `--accent*`,
  `--brand-text-light/dark`, `--font-sans`, `--font-heading`) and the radius
  scale under `:root[data-template]`. `v` = sha256(tenant id + CSS). The
  current `v` → `public, max-age=31536000, immutable`; any other or none →
  `public, max-age=300`; `ETag` / 304. The tenant comes from `Host`, so a
  page only ever references its own version. It is a render-blocking
  same-origin `<link>` after `app.css` and the template sheet (CSP stays
  `style-src 'self'`; nothing inline). CSS layers:
  `reset, tokens, base, layout, components, utilities, pages, template`; the
  unlayered theme sheet wins.
* Colour maths (`color.rs`, OKLab / OKLCH, server side): text on a brand
  fill is black or white, whichever contrasts more (AA for every sRGB
  colour; a test sweeps the RGB cube); hover moves away from the ink; the
  gradient partner and the light / dark text variants keep AA.
* Templates (`presets.rs`), each with its own home hero partial
  (`templates/themes/<key>/home_hero.html`, shared pieces in
  `themes/_hero_actions.html` / `_tld_chips.html`), drawn SVG art (token
  colours only, no images) and a share-image style:

  | key | look | fonts | art / share image |
  |---|---|---|---|
  | `classic` | neutral, hairline cards, split hero (no extra CSS) | Geist | server rack + TLD price panel / grid |
  | `cloud` | centred hero over a brand mesh, pill search, soft cards | Inter / Plus Jakarta Sans | control-panel preview / orbs |
  | `corporate` | navy hero, utility bar, square cards, domain card | IBM Plex Sans | network map / grid |
  | `terminal` | mono accents, console TLD table, green accents | Inter / Space Grotesk + Geist Mono | console window / terminal |
  | `studio` | warm, rounded, offset-shadow pill buttons, playful shapes | Figtree / Outfit | browser + sun + blob / blocks |
  | `editorial` | paper and ink, Fraunces display serif, hairline rules, roman numerals | DM Sans / Fraunces | circle ornament + TLD index / paper |

  Each has `static/css/themes/<key>.css` (classic has none)
  (tokens in `@layer tokens` for light, `[data-theme="dark"]` and the
  `prefers-color-scheme` query; components in `@layer template`, every
  selector under `[data-template="<key>"]`) and, where structure differs,
  partials in `templates/themes/<key>/` included with
  `{% if tpl() == "<key>" %}` (home hero, corporate utility bar, cloud /
  studio / editorial footer wordmark). Page templates stay shared. To add
  one: a `PRESETS` entry (fonts, radius, mode, colours, `share_style`,
  480×300 thumbnail in `static/img/themes/<key>.webp`, a 1440×900 capture
  of the home page scaled down), the CSS file, the hero partial,
  `admin_settings.appearance.templates.<key>.{name,description}` in both
  locales, and the key in `TEMPLATES` of `tests/it/appearance.rs` and
  `tests/it/storefront_seo.rs`. Check 320–1920 px, light and dark, reduced
  motion and any brand colour (pale and dark).
* Share images: `GET /og/<key>.png?v=<theme version>` is a 1200×630 PNG
  drawn in pure Rust (`hos_seo::og`, own DEFLATE + CRC) from the theme's
  brand / accent colours, the template's style and a per-page seed
  (`Theme::share_image_url`). `seo::apply` uses it whenever a page has no
  image of its own; the sitemap lists it as the page's image. Only keys of
  real pages render (fixed routes and published content,
  `content::share_key_exists`; anything else is `404`), per-IP limited
  (`Limits::og`) and kept in a bounded LRU in process; `immutable` for the
  current version, `no-store` in a preview.
* Fonts (`fonts.rs`): variable WOFF2, `latin` + `latin-ext` subsets in
  `static/fonts/<key>/` with `OFL.txt` and `SOURCE.txt`. To add a family:
  the files and licence, a `FONTS` entry with its weight range and fallback
  metrics (`size-adjust` = width ratio of a TR + EN sample against Arial or
  Georgia at 400 and 600, plus ascent / descent; measure in a browser). The
  registry test checks files and licences. Pages preload only the active
  families (`latin`, plus `latin-ext` on Turkish pages) and Geist Mono.
* Preview: `?theme_preview=1&tpl=&brand=&accent=&fb=&fh=&r=&m=&tg=` on any
  customer page applies unsaved values for sessions with `settings.manage`
  only (anyone else gets the normal page). Such a response is
  `private, no-store`, `noindex` and framable by the same origin only
  (`frame-ancestors 'self'`, `X-Frame-Options: SAMEORIGIN`); the Appearance
  page shows it in a scaled iframe (`admin_appearance.js`).

## Forms, CSRF, flash

* Every state-changing request needs a CSRF token. Form posts carry
  `{% call ui::csrf() %}{% endcall %}`. htmx requests send `X-CSRF-Token`
  automatically (`app.js` reads `<meta name="csrf-token">`). Tokens are bound
  to the session, or to the `hos_anon` cookie before login. A cross-origin
  `Origin` header is rejected. `/webhooks/*` and `/api/*` requests with an
  `Authorization: Bearer <api key>` are exempt; an API call authenticated by
  the session cookie (any other or no `Authorization`) needs the token.
* Validate into `FormErrors`. On error, re-render with **422**
  (`render_status(StatusCode::UNPROCESSABLE_ENTITY, &page)`). On success,
  **redirect** (Post/Redirect/Get) with a flash message.

```rust
let mut errors = FormErrors::default();
if !forms::valid_email(&form.email) { errors.add("email", ctx.t("auth.errors.email_invalid")); }
if errors.any() { return render_status(StatusCode::UNPROCESSABLE_ENTITY, &page); }
Ok(flash::redirect(&ctx.url("/client/account"), SetFlash::success(ctx.t("client.account.saved"))))
```

```html
<form method="post" action="{{ url("/client/account") }}" class="stack">
  {% call ui::csrf() %}{% endcall %}
  {% call ui::form_errors(errors) %}{% endcall %}
  {% call ui::field("email", t("auth.fields.email"), form.email, kind="email", error=errors.get("email"), required=true) %}{% endcall %}
  <button class="btn btn--primary" type="submit">{{ t("common.actions.save") }}</button>
</form>
```

* Public forms add `{% call ui::honeypot() %}{% endcall %}` and check
  `forms::honeypot_tripped(form.website.as_deref())`. They also apply a rate
  limit: `check_rate(&state.limits.contact, &ip.key("contact"))?`. Add a new
  limiter to `state::Limits`.
* Flash messages survive exactly one redirect (signed cookie) and render as
  toasts in `base.html`.

## Authorization

* Area guards: `/client` → `require_client`, `/admin` → `require_staff`,
  `/platform` → `require_platform_admin`. Anonymous users get a 401, and
  browsers are redirected to the login page with `?next=`. The wrong kind of
  user gets a 403.
* Fine-grained admin checks go in the handler: take
  `RequirePermission<perm::ClientsManage>`, or `staff.require(Permission::X)?`
  for conditional checks. Hiding a menu entry or button (`ctx.can("…")`) is
  never the check itself.
* Every query is tenant-scoped (`tenant.id` is always the first argument of
  hos-db repo functions). Never take a tenant id from the request.
* Client sub-users: `client_users.permissions` (`MemberAccess`, areas
  `services`, `domains`, `invoices`, `orders`, `tickets`, `profile`,
  `contacts`; `owner` = account owner only) are enforced by the
  `member_access` layer on `/client/*` (`client::area_of`), by
  `client::require_member_area` on `/checkout/*` (`orders`) and by the API
  gate (`api::member_areas`, every API route is mapped; a unit test checks
  it). API keys are owner-only (`/client/account/api-keys`) and a key acts
  with its user's membership as it is at request time.
* Unverified e-mail (`auth.require_email_verification`, default on): no
  session for an unverified account (the login re-sends the link), no OAuth
  linking and no API keys until the e-mail is verified. When the mailbox
  owner reclaims such an account (OAuth sign-in with a verified e-mail,
  password reset), `auth::revoke_unverified_account` drops its 2FA,
  sessions, other linked identities, API keys and the sub-users it invited
  (`client_users.invited_by`).
* API: the `gate` middleware authenticates (`ApiAuth`: key hash, expiry,
  revocation, allowed IPs, key scopes ∩ role permissions; or the session —
  refused while a staff user still has to set up required 2FA), checks the
  client member areas, rate-limits and handles `Idempotency-Key`. Handlers call
  `auth.scope(Permission::X)?` (staff with X, or a client restricted to its
  own records via `scope.owns(…)` / `list_query`) or `auth.staff(Permission::X)?`
  (staff only). See "REST API" below.

## i18n

* Files: `locales/<tr|en>/<area>.json`. The file stem is the first key
  segment (`client.json` → `client.*`). Keys are nested objects. Values use
  `{name}` placeholders. A plural is an object with `one`/`other` (optional
  `zero`) and `{count}`.
* Both catalogs must have identical key sets (unit test). Turkish text uses
  real diacritics.
* Storefront URLs: the default locale has no prefix, the other one is
  prefixed (`/en/...`). A `/tr/...` request for the default locale is
  redirected with a 308. App areas follow the user's saved locale. Always
  build links with `url()`/`ctx.url()`.
* Money and dates use the tenant's currency and time zone:
  `money`, `date`, `i18n::format_month`.

## htmx fragments

The page renders a full, working HTML form or link (no-JS fallback). It adds
`hx-get`/`hx-post` + `hx-target`/`hx-swap`. The handler takes `Htmx` and
returns a fragment when `htmx.request` is true, otherwise it redirects or
renders the full page. Name fragment templates with a leading underscore
(`storefront/_domain_row.html`). Examples are the KB vote
(`storefront/kb.rs::vote`) and the lazily loaded domain rows
(`domains.rs::check`, `hx-trigger="load"`).

## Tables and pagination

Take `PageQuery` (`page`, `per_page` ≤ 100, `sort`, `dir`, `q`). Map `sort`
through an allow-list with `q.sort_column(&[("name", "c.name"), …], "c.created_at")`
and never interpolate request input into SQL. Repo functions return
`Paged<T>`. Build the view model with `Pager::new(&paged, &ctx.url(base),
&[("q", …)])` and render it with `{% call ui::pagination(pager) %}{% endcall %}`.
Use this markup:

```html
<div class="table-wrap"><table class="table">
  <thead><tr><th>…</th><th class="num">…</th></tr></thead>
  <tbody>…</tbody>
</table></div>
```

Use `{% call ui::empty(...) %}` when there are no rows.

## Errors

Handlers return `AppResult<Response>`. The `?` operator converts hos-db and
hos-core errors (`CoreError`) by their status: 404 becomes `NotFound`, 409
becomes `Conflict`, 502–504 become `Unavailable`. `sqlx::Error`,
`anyhow::Error`, `AuthError`, `IntegrationError` and `CryptoError` also
convert. Unexpected errors become `Internal`, which is logged with the
request id. Use these variants:

* `NotFound`
* `Unauthorized`
* `Forbidden(log_msg)`
* `BadRequest(safe_msg)`
* `Conflict(safe_msg)`
* `RateLimited { retry_after }`
* `PayloadTooLarge`
* `Unavailable(log_msg)` for an upstream integration that is down
* `Internal(log_msg)`

The `error_pages` middleware renders a localized HTML page (`errors/error.html`).
It returns `application/problem+json` instead for `/api/*`, `/webhooks/*` or
`Accept: application/json`. Internal messages are never shown to users.
Panics become a 500 page.

## Tenant-less public routes

Some endpoints run outside the tenant stack (no `Host` → tenant lookup, no
session, no CSRF); each area exposes them through `public_router(state)`,
merged in `routes::app` before the tenant fallback:

* `webhooks::public_router` — `/webhooks/gateway/{integration_id}` and
  `/webhooks/registrar/{integration_id}` (GET + POST). The tenant comes from
  the integration row; raw body, headers and query go unchanged into
  `Billing::verify_webhook` and then `Billing::apply_webhook`. Order: a
  per-IP limit of failed requests (`Limits::webhook_failed`, client IP from
  `ClientIp`), then the signature check (gateways that verify by calling the
  provider, e.g. PayPal, first take `Limits::webhook_remote`), and only
  verified calls take the per-integration `Limits::webhook` bucket, so forged
  calls cannot use it up. A verified call is recorded in `webhooks_in` with
  card fields removed (`hos_payments::raw_for_storage`); a failed one only as
  body / query size and SHA-256 (at most 200 such rows per integration).
  `hostingos webhooks scrub` cleans rows stored before that.
  `/webhooks/mail/mailgun-mime` and `/webhooks/mail/raw` (inbound e-mail)
  follow the same order; a sender counts as a known client only after
  sender authentication (`inbound::authenticate_sender`).
* `platform::public_router` — `GET /internal/tls-allow?domain=` for the
  reverse proxy's on-demand TLS (loopback or `HOS_TLS_ASK_SECRET`).
* `api::public_router` — `/api/v1/openapi.json` and `/api/docs`.

Errors there are written as `application/problem+json` directly.

## REST API

* Declare a route once in `routes/api/mod.rs::build()` **and** add the
  matching `Operation` to `routes/api/openapi.rs::OPERATIONS`; a unit test
  compares both lists.
* Handlers return `ApiResult<ApiReply>`; `ApiError` is the problem document
  (`ApiError::field("x", "…")` / `validation(vec![…])` give `422` with
  field errors). Bodies arrive as `Bytes` → `parse_body::<T>()`.
* Lists: `list_query(&raw, SORTS, STATUSES, scope)` (limit/offset, whitelisted
  `sort`, `status`, `q`, `client_id`, `created_from/to`) → `hos_db::repo::api`
  → `paged(&page, dto::x)`. Money via `dto::money` (string, two places) next
  to `currency`.
* Business actions call hos-billing (`checkout`, `start_payment`,
  `record_manual_payment`, `request_suspend`, `vm_*`, `domain_*` …).

## Plan limits

`routes::platform::limits::check_plan_limit(&state, tenant_id, Limit::X)`
returns `AppError::Conflict` when the tenant's plan (or its per-tenant
override from the platform console) is exhausted; `plan_limit_status` gives
`used / max` for display. Limits: `Clients` (`max_clients`), `Services`
(`max_services`), `Staff` (`max_staff`), `CustomDomains`, `Integrations`.
Call it right before creating the counted record. Wired today: API client
creation and API orders. Still to wire: client sign-up / admin client
creation (`Clients`), storefront checkout + admin orders (`Services`), staff
invites (`Staff`), tenant custom domains in admin settings
(`CustomDomains`), admin integration creation (`Integrations`).

## Storefront SEO

Every public page builds a `SeoPage`, which takes a title, path, description,
JSON-LD nodes (`node(&Product{…})`) and crumbs, then calls `seo::apply`.
Organization and WebSite nodes, canonical, hreflang (`tr`, `en`,
`x-default`), OG/Twitter tags (generated share image) and SEO overrides from
`seo_meta` are added automatically. Pages with Markdown content also serve a
`.md` twin announced with `<link rel="alternate" type="text/markdown">`. Add
new public URL types to `storefront/files.rs` (sitemap, llms.txt,
llms-full.txt). Rules the routes follow: FAQPage only for questions visible on
the page, AggregateRating only from real reviews (none are invented),
Article / BlogPosting / TechArticle with the organisation as author, HowTo
for KB articles with numbered steps, a visible `<time>` "last updated" date.
Check a page with:

```sh
curl -s http://localhost:8080/ > /tmp/p.html
cargo run -p hos-web --example seo_audit -- /tmp/p.html http://localhost:8080/ tr "Brand" "keyword"
```

### Default content and blocks

New tenants get a default page set (`hos_db::seed::site`, tr + en, keyed on
tenant + locale + slug, never overwritten): legal templates (terms,
privacy / KVKK, refund, SLA; each opens with a staff-only review note), about,
infrastructure, hosting-types comparison, status, and category landings
(web hosting, WordPress, VPS, dedicated, SSL, e-mail, reseller) as drafts until
the tenant has that product group. The demo seed publishes them. All of it
is ordinary CMS pages (`/admin/content/pages`):

* Slugs `home`, `domains`, `contact` hold the copy of those routes (hero,
  lead, sections); the page itself 301s to the route and is left out of the
  sitemap and llms.txt. A page whose slug equals a product group's slug is
  that category's landing: its first paragraph is the hero lead, its blocks
  render under the plans.
* Content blocks (`routes/storefront/blocks.rs`, rendered by
  `components/blocks.html`) are Markdown directives:

  ```text
  :::features Title            ### :icon: Item title + text, one per ###
  :::steps Title               ### Step title + text (numbered)
  :::faq Title                 ### Question? + answer (FAQPage JSON-LD)
  :::compare Title             a Markdown table; ✓ / ✗ cells styled
  :::spec Title                a two-column Markdown table
  :::pricing group-slug        the group's live plans with cycle toggle
  :::tlds                      the live TLD price table
  :::cta Title                 text + [links](…) as buttons
  :::hero Title                home: headline, text, [links](…)
  :::note / :::todo            callout / staff-only reminder
  :::
  ```

  Unknown or unterminated directives stay plain text. Tokens:
  `{{site_name}}`, `{{legal_name}}`, `{{email}}`, `{{support_email}}`,
  `{{phone}}`, `{{address}}`, `{{year}}`, `{{url}}` and `{{base}}` (locale
  prefix for internal links: `[VPS]({{base}}/vps)`). Never write tenant
  claims (uptime figures, customer counts, awards, testimonials) into
  defaults; leave a `:::todo` for the tenant instead.

## Tests

* Unit tests sit next to the code (`cargo test -p hos-web --lib`).
* Integration tests form one test binary, `tests/it/` (`cargo test -p hos-web
  --test it`; one group with `--test it storefront_seo::`): `web.rs`,
  `api.rs`, `webhooks.rs`, `platform.rs`, `appearance.rs`, `storefront_seo.rs`
  (every template × key page: one `<h1>`, canonical, hreflang pairs, JSON-LD
  fields, no duplicate ids; sitemap, robots, llms, default pages …) and the
  others are modules of `tests/it/main.rs`, with helpers in
  `tests/it/common.rs`. Add new test files there as modules, not as new
  `tests/*.rs` files: every extra test binary is another full link of
  hos-web and its dependencies (hundreds of MB in `target/`).
  `TestApp::new()` creates a throw-away tenant reached through
  `Host: <slug>.<HOS_BASE_DOMAIN>`. It has a cookie jar, `get`/`post`,
  `res.csrf()` and `res.json_ld()`, and `cleanup()`, which deletes the tenant
  and cascades. Without a configured database the tests print a notice and
  pass. The harness sets the outbound policy to `Unrestricted` (providers
  run against wiremock on 127.0.0.1) and opts in to the mock integrations;
  build extra states with `common::new_state`, not `AppState::new`. The
  harness also calls `hos_db::repo::jobs::isolate_queue("tests")`: the
  test process enqueues and claims in the `tests.` queue namespace, which a
  `hostingos serve` worker on the same database never claims (it would run
  the jobs with its own secret key and fail them), while
  `run_tenant_until_idle` claims a tenant's jobs whatever their queue.

## Checklist for a new area

1. Create a module under `src/routes/<area>/` with `router(state)`. Merge it
   in `routes::routes` and put the guard on it with `route_layer`.
2. Add `menu.rs` (`MenuSectionDef`s) and `chrome()`, then set
   `ctx.panel = Some(menu::chrome(...))` in each handler.
3. Put templates in `templates/<area>/`, extending `layouts/panel.html`.
4. Add `locales/{tr,en}/<area>.json` (or extend the existing file for the area).
5. Add repo functions in `hos-db` (tenant-scoped, runtime `sqlx::query`).
   Do not write SQL in handlers.
6. Add integration tests with `TestApp`, then run `cargo fmt`,
   `cargo clippy -p hos-web --all-targets -- -D warnings` and
   `cargo test -p hos-web`.
