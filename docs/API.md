# HostingOS REST API (v1)

JSON over HTTPS on the hosting company's own address (`HOS_BASE_URL`), for
example `https://panel.example.com/api/v1/…`. (In the optional multi-company
mode each company's API is on its own host, and a key of one company is
unknown on every other host.)

* Machine-readable description: `GET /api/v1/openapi.json` (OpenAPI 3.1,
  every route included — a unit test compares it with the router).
* Human-readable reference: `GET /api/docs` (self-contained, no CDN).

## Authentication

```
Authorization: Bearer hos_live_<lookup>_<secret>
```

| Key owner | Created in | Sees |
|---|---|---|
| Staff user | Admin panel (API keys) | Everything its role allows; a key with scopes gets *scopes ∩ role permissions* |
| Client user | Client area → Account → API keys | Only its own client's records; other ids answer **404** (not 403) |

Keys can expire, be revoked and be limited to IP ranges. A browser session
cookie also works (same permissions); unsafe methods then need the
`X-CSRF-Token` header (the `csrf-token` meta tag of any panel page).

`GET /api/v1/me` shows who you are and, for staff, the effective
permissions. Permission names: `clients.view|manage`, `billing.view|manage`,
`products.manage`, `services.view|manage`, `domains.manage`,
`support.view|manage`, `integrations.manage`, `reports.view`, …

| Status | Meaning |
|---|---|
| 401 `unauthorized` | no / invalid / expired / revoked key |
| 403 `forbidden` | authenticated, but the permission is missing or the endpoint is staff-only |
| 404 `not_found` | does not exist — or belongs to another client |

## Conventions

**Errors** are RFC 9457 problem documents (`Content-Type:
application/problem+json`):

```json
{
  "type": "urn:hostingos:problem:validation_failed",
  "title": "Unprocessable Entity",
  "status": 422,
  "code": "validation_failed",
  "detail": "The request has invalid fields.",
  "errors": [{ "field": "items[0].billing_cycle", "message": "required for product items" }]
}
```

**Money** is a decimal string with two places next to an ISO 4217 code:
`{"total": "538.80", "currency": "TRY"}` — never a float.

**Lists** take `limit` (1–200, default 25), `offset`, `sort` (whitelisted per
resource, `-` prefix = descending, default newest first), `status`, `q`
(search), `client_id` (staff), `created_from` / `created_to` (RFC 3339) and
answer

```json
{ "data": [ … ], "pagination": { "total": 3, "limit": 2, "offset": 0, "has_more": true, "next_offset": 2 } }
```

Unknown sort keys or bad parameters → `422` naming the field.

**Idempotency** — send `Idempotency-Key: <uuid>` on any `POST`. The first
response (status < 500) is stored for 24 hours per key and caller; a retry
returns it again with `Idempotent-Replayed: true`. The same key with a
different body → `422 idempotency_key_reused`; while the first request is
still running → `409 idempotency_in_progress`.

**Rate limits** — per key (or user / IP): `RateLimit-Limit`,
`RateLimit-Remaining`, `RateLimit-Reset` (seconds) and `RateLimit-Policy` on
every response; `429` with `Retry-After` when exhausted. The limiter is
in-memory per process (see DEPLOYMENT.md, scaling).

## Resources

| Area | Endpoints |
|---|---|
| Meta | `GET /ping` (public), `GET /me`, `GET /stats/dashboard` |
| Catalog | `GET /catalog` (public, `?currency=`; groups → products → prices + configurable options), `GET /tlds` (public), `GET /products`, `GET /products/{id}` |
| Clients | `GET/POST /clients`, `GET/PATCH /clients/{id}` |
| Orders | `GET/POST /orders`, `POST /orders/quote`, `GET /orders/{id}` |
| Quotes | `GET /quotes`, `GET /quotes/{id}` (staff `quotes.view`; a client key sees its own sent quotes), `POST /quotes/{id}/accept · decline` (client; a second answer → `409`) |
| Invoices | `GET /invoices`, `GET /invoices/{id}`, `GET /invoices/{id}/pdf`, `POST /invoices/{id}/pay`, `POST /invoices/{id}/payments` (manual, staff), `GET /transactions` |
| Services | `GET /services`, `GET /services/{id}`, `POST …/suspend · unsuspend · terminate` (staff, queued → 202), `POST …/power`, `POST …/reinstall`, `GET/POST …/snapshots`, `POST …/snapshots/{snapshot}/rollback`, `DELETE …/snapshots/{snapshot}`, `POST …/console`, `POST …/password-reset`, `GET …/backups`, `POST …/backups/{backup}/restore`, `GET/PUT …/reverse-dns`, `GET …/isos`, `POST/DELETE …/iso`, `POST …/rescue`, `GET/PUT …/firewall`, `GET …/metrics · bandwidth · operations` |
| SSH keys | `GET/POST /ssh-keys`, `DELETE /ssh-keys/{id}` |
| Domains | `GET /domains`, `GET /domains/availability?domain=` (public), `POST /domains/register`, `GET /domains/{id}`, `POST …/renew`, `PUT …/nameservers`, `PUT …/lock` |
| Support | `GET /departments`, `GET/POST /tickets`, `GET /tickets/{id}`, `GET/POST /tickets/{id}/messages` |
| Status | `GET /status`, `GET /status/incidents`, `GET /status/incidents/{id}` (public, no key) |
| Affiliate | `GET /affiliate`, `GET /affiliate/commissions · referrals · payouts` (client keys, account owner only) |
| Tasks | `GET/POST /tasks`, `GET/PATCH/DELETE /tasks/{id}` (staff `tasks.manage`) |
| Integrations | `GET /integrations` (metadata only — never config or secrets) |
| Webhooks out | `GET/POST /webhook-endpoints`, `DELETE /webhook-endpoints/{id}` |

All paths are below `/api/v1`. Business rules (pricing, coupons, tax,
invoices, provisioning, registrar calls) run in hos-billing — the API is a thin
layer over the same code the panels use.

## Examples

```sh
H=https://acme.example.com
KEY=hos_live_xxxxxxxxxx_yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy

# Catalog with option ids (public)
curl -s $H/api/v1/catalog?currency=TRY

# Place an order for a client (staff key with billing.manage; a client key omits client_id)
curl -s $H/api/v1/orders \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -H "Idempotency-Key: $(uuidgen)" \
  -d '{
        "client_id": "01a1e618-3d75-77a5-a290-7f79a32a9fc0",
        "items": [{
          "product_id": "01a1e5c1-e014-7103-8fcf-00c3eb09da0f",
          "billing_cycle": "monthly",
          "hostname": "vps1.example.net",
          "config_options": {
            "<os option id>": { "value_id": "<ubuntu value id>" }
          }
        }, {
          "domain": "example.net", "action": "register", "years": 1
        }]
      }'
# → 201 {"order_id", "order_number", "invoice_id", "service_ids", "domain_ids",
#        "total": "538.80", "currency": "TRY", "paid": false,
#        "next": {"pay": "/api/v1/invoices/<id>/pay"}, "quote": {...}}

# Start the payment; the answer says how to continue. `return_url` /
# `cancel_url` must be on the shop's own hosts (its address and domains) or
# in the setting
# `payments.return_url_hosts` (Admin → Settings → Billing → Online payments:
# exact host names, one per line); others are refused.
curl -s -X POST $H/api/v1/invoices/<invoice id>/pay \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"return_url": "https://acme.example.com/thanks"}'
# → {"session": {"type": "redirect", "url": "https://checkout.provider/..."}}
#   or {"type": "form", "action": "...", "method": "POST", "fields": [{name, value}...]}
#   or {"type": "offline", "instructions": "..."}

# Offline payment recorded by staff
curl -s $H/api/v1/invoices/<invoice id>/payments \
  -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"amount": "538.80", "method": "bank_transfer", "reference": "EFT-2026-114"}'

# Active services of the caller, newest first
curl -s "$H/api/v1/services?status=active&sort=-created_at&limit=50" -H "Authorization: Bearer $KEY"

# VM actions
curl -s $H/api/v1/services/<id>/power -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/json" -d '{"action": "reboot"}'

# Domain availability (public) and nameservers
curl -s "$H/api/v1/domains/availability?domain=example.net"
curl -s -X PUT $H/api/v1/domains/<id>/nameservers -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/json" -d '{"nameservers": ["ns1.example.net", "ns2.example.net"]}'

# Ticket
curl -s $H/api/v1/tickets -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"subject": "Reverse DNS", "message": "Please set PTR for 203.0.113.11", "priority": "medium"}'
```

## Network status (public)

The same data as the `/status` page — only public components and public
incidents leave the server:

```sh
curl -s $H/api/v1/status
# → {"level": "ok" | "minor" | "major" | "maintenance",
#    "components": [{"id", "name", "group", "description", "status", "updated_at"}],
#    "incidents": [ open incidents and maintenance ]}

curl -s $H/api/v1/status/incidents            # last 90 days, newest first (max 100)
curl -s $H/api/v1/status/incidents/<id>       # one incident with its "updates" timeline
# incident: {"id", "kind": "incident" | "maintenance", "title", "impact", "status",
#            "scheduled_start", "scheduled_end", "started_at", "resolved_at",
#            "component_ids", "updated_at"}
```

## Affiliate account (client keys)

Read-only view of the caller's own affiliate account — what
`/client/affiliate` shows. Only client keys of the account owner; a staff key
gets `403`, a client without an affiliate account `404`.

```sh
curl -s $H/api/v1/affiliate -H "Authorization: Bearer $KEY"
# → {"id", "code", "status", "currency", "link": "https://<host>/r/<code>",
#    "referrals": 3, "clicks": 41, "pending": "12.00", "balance": "30.00", "paid_out": "10.00", …}

curl -s $H/api/v1/affiliate/commissions -H "Authorization: Bearer $KEY"   # newest 100
curl -s $H/api/v1/affiliate/referrals   -H "Authorization: Bearer $KEY"   # names masked
curl -s $H/api/v1/affiliate/payouts     -H "Authorization: Bearer $KEY"
```

## Staff tasks

The admin to-do list (`tasks.manage`, staff keys only). Lists take
`?status=open|done`, `?assigned_to=<user id>` and `?client_id=`.

```sh
curl -s $H/api/v1/tasks -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
  -d '{"title": "Call back about the migration", "priority": "high",
       "due_date": "2026-12-01", "assigned_to": "<staff user id>", "client_id": "<client id>"}'
# → 201 {"id", "title", "body", "status": "open", "priority", "due_date", "assigned_to",
#        "client_id", "created_by", "completed_at", "created_at", "updated_at"}

# Mark done; null clears a field (due_date, assigned_to, client_id, body)
curl -s -X PATCH $H/api/v1/tasks/<id> -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/json" -d '{"status": "done", "due_date": null}'
```

`priority` is `low`, `normal` (default) or `high`. An assignee that is not a
staff user, or a client that does not exist, is a `422`.

## Incoming e-mail (ticket piping)

Enabled and configured in the admin under `/admin/support/inbound/settings`. Two
CSRF-exempt endpoints on the company's host:

```
POST https://<host>/webhooks/mail/mailgun-mime   Mailgun inbound route, forward("…/mailgun-mime")
POST https://<host>/webhooks/mail/raw            raw RFC 5322 body, Authorization: Bearer <token>
                                                        optional ?recipient=…&sender=…
```

The Mailgun endpoint checks the webhook signature (HMAC-SHA256 of
`timestamp + token` with the HTTP webhook signing key, recent timestamps,
each token once — remembered in the database, so a replay is refused by every
process and after a restart). The raw endpoint takes the token shown once in
the admin (only its hash is stored). A request that fails authentication
counts against a per-IP limit; only authenticated mail uses the company's
inbound limit.

The `From` header alone never decides which client wrote: a sender counts as
a known client only when sender authentication passed for the From domain.

* Raw endpoint: the topmost `Authentication-Results` header whose authserv-id
  is the one set under "Trusted Authentication-Results server" (your
  receiving MTA, which must strip forged copies of its own header), with
  `dmarc=pass`, or an aligned `dkim=pass` / `spf=pass`.
* Mailgun: Mailgun's signature covers only `timestamp + token`, not the
  message; its `X-Mailgun-Spf` header exists only when the Mailgun domain's
  spam filter is set to "mark spam with MIME headers". Turn on "Trust
  Mailgun's SPF result" only then; `Pass` for an envelope sender aligned with
  the From domain authenticates the sender. Mailgun's DKIM result names no
  domain and is not used.

Unauthenticated mail from a client address follows the unknown-sender
setting (quarantine by default). Replies are threaded only by the reply
token in the plus address (`support+t<token>@…`); a ticket number in the
subject without a token is quarantined. Ticket views mark messages that
arrived by e-mail and those whose sender was not verified. Automated mail
and floods are refused and a Message-ID is processed once. Every message is
logged at `/admin/support/inbound` (quarantined ones can be released, added
to a ticket or discarded).

## Incoming webhooks (gateways, registrars)

Configure the provider to call

```
https://<host>/webhooks/gateway/<integration id>
https://<host>/webhooks/registrar/<integration id>
```

The integration id identifies the integration — the host does not matter. GET and
POST are accepted; body, headers and query string reach the gateway module
unchanged and are verified there (signature / hash per provider). Answers:
the provider-specific reply (`OK` text, `303` redirect for browser post-backs,
empty `200`); `4xx` for bad signatures or unknown integrations (no retry
wanted); `5xx` + `Retry-After` for transient failures (the provider should
retry). Duplicate deliveries are recognised and applied once. Every request
is stored in `webhooks_in` (secrets in headers are dropped).

## Outgoing webhooks

Register an endpoint (the `secret` is returned only once):

```sh
curl -s $H/api/v1/webhook-endpoints -H "Authorization: Bearer $KEY" \
  -H "Content-Type: application/json" \
  -d '{"url": "https://hooks.example.net/hostingos", "events": ["invoice.paid", "service.activated"]}'
```

Events include `order.created`, `order.accepted`, `invoice.created`,
`invoice.paid`, `payment.refunded`, `service.activated`, `service.suspended`,
`service.unsuspended`, `service.terminated`, `service.provisioning_failed`,
`domain.registered`, `domain.renewed`, `domain.transferred` (an empty list or
`*` subscribes to all). Each delivery is

```
POST <url>
Content-Type: application/json
X-HostingOS-Event: invoice.paid
X-HostingOS-Event-Id: <uuid>
X-HostingOS-Signature: t=1763330000,v1=<hex>

{"id": "<uuid>", "event": "invoice.paid", "created_at": "…", "tenant_id": "…", "data": {…}}
```

`v1` is `hex(HMAC-SHA256(secret, "<t>.<raw body>"))`. Verify it on the raw
bytes, compare in constant time and reject old timestamps (e.g. > 5 minutes):

```python
import hmac, hashlib, time

def verify(secret: str, header: str, body: bytes, tolerance: int = 300) -> bool:
    parts = dict(p.split("=", 1) for p in header.split(","))
    t, sig = parts.get("t"), parts.get("v1")
    if not t or not sig or abs(time.time() - int(t)) > tolerance:
        return False
    mac = hmac.new(secret.encode(), f"{t}.".encode() + body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(mac, sig)
```

```js
// Node.js
const crypto = require("crypto");
function verify(secret, header, rawBody, tolerance = 300) {
  const p = Object.fromEntries(header.split(",").map(s => s.split("=", 2)));
  if (!p.t || !p.v1 || Math.abs(Date.now() / 1000 - Number(p.t)) > tolerance) return false;
  const mac = crypto.createHmac("sha256", secret).update(`${p.t}.`).update(rawBody).digest("hex");
  return mac.length === p.v1.length && crypto.timingSafeEqual(Buffer.from(mac), Buffer.from(p.v1));
}
```

Answer `2xx` quickly; anything else is retried with exponential backoff.
Use `X-HostingOS-Event-Id` to ignore duplicates.
