# Importing from WHMCS

HostingOS can take over a WHMCS installation: clients and their logins,
products, services, domains, invoices, payments, tickets, the knowledge base
and more. The import runs into the company, can be previewed (dry run) without
writing anything, can be resumed and cancelled, and can be rolled back.

* Web: **Admin → System → Data import** (`/admin/tools/import`), needs the
  `import.manage` permission (only the `owner` role has it by default).
* CLI: `hostingos import whmcs|list|rollback`.
* Code: `crates/hos-import` (connectors, mapping, engine, rollback, report),
  `crates/hos-web/src/routes/admin/import` (wizard and CLI),
  `crates/hos-auth/src/legacy.rs` (imported password hashes),
  `crates/hos-db/src/repo/import*.rs`, migration `0029_import.sql`.

## Connectors

| | Database (recommended) | API |
|---|---|---|
| Access | read-only MySQL / MariaDB user on the WHMCS database, plus `cc_encryption_hash` from `configuration.php` | an API credential (identifier + secret, optional access key) |
| Client / user password hashes | imported (bcrypt, legacy MD5) | not exposed → those users reset their password |
| Hosting account and server passwords | decrypted with `cc_encryption_hash` (optional) | not exposed |
| Tax rules, client groups, client notes, configurable options, product add-on catalog, knowledge base | imported | **skipped: unsupported** |
| Quotes, affiliates (accounts, referrals, commission history, withdrawals, balances), to-do list, banned IPs / e-mail domains, downloads, network issues | imported | **skipped: unsupported** (the API does not expose all of it) |
| Ticket attachments | WHMCS keeps them on disk: give API credentials in the "attachments" section, otherwise they are reported as skipped | `GetTicketAttachment` |

The database connector never reads card, bank account, 2FA, security
question or reset-token columns (`DENIED_COLUMNS` in
`hos-import/src/source/mod.rs`); card tables are not read at all.

Both connectors produce rows with the WHMCS column names, so every entity has
one mapper (`hos-import/src/engine/handlers.rs`, pure helpers in `map.rs`).

### What is imported

Currencies, tax rules, client groups, servers (as **inactive** integrations),
staff (**disabled**, unverified and without passwords; WHMCS roles mapped in
the wizard), clients, user
accounts and their client links (owner / member permissions), contacts,
client notes, product groups, products and prices, configurable options,
product add-ons, promotions (coupons), TLDs and their prices, invoices and
items (the amount paid of open invoices from their transactions, see
below), credit balance (reconciled to `tblclients.credit`), orders, services
(with option selections, custom fields, add-ons, cancellation requests),
domains, transactions, support departments, tickets, replies, notes and
attachments, canned replies, knowledge base, announcements, and (database
connector) quotes, the affiliate programme, the to-do list, ban lists,
downloads and network issues — see below — and (optionally) the newest
activity-log lines.

Everything without a HostingOS counterpart is listed in the report as
**skipped: unsupported**: e-mail templates, stored payment methods, server
groups, billable items and projects.

### Quotes, affiliates, to-dos, bans, downloads, network issues

Database connector only (`hos-import/src/engine/extras.rs`, mappers in
`map.rs`):

| WHMCS | HostingOS | Notes |
|---|---|---|
| `tblquotes` | `quotes` | Client quotes keep the client's currency; quotes to a non-client become prospect quotes (name, company, e-mail). Stage: Draft → `draft`, Delivered / On Hold → `sent` (`expired` once past *valid until*), Accepted → `accepted`, Lost → `declined`, Dead → `cancelled`. Proposal and customer notes become the notes, admin notes stay internal. New quotes get the next quote number. A quote with neither an imported client nor a valid e-mail is skipped (`no_recipient`). |
| `tblquoteitems` | `quote_items` | Quantity × unit price less the discount %, rounded half up to cents (as in HostingOS). Negative prices are skipped. |
| `tblaffiliates` | `affiliates` | One account per client (an account the client already opened in HostingOS is adopted). Pay type percentage / fixed amount → commission override; "one time" → not recurring. A new referral code is generated (WHMCS `aff=<id>` links stop working). |
| `tblaffiliatesaccounts` | `affiliate_referrals` | The referred client is the owner of the referred service; several services of one client are one referral. A client referred by another affiliate is skipped. |
| `tblaffiliateshistory` | `affiliate_commissions` | Positive lines with an imported referral; reversals and lines without a referral are reported, not imported. |
| `tblaffiliateswithdrawals` | `affiliate_payouts` | Paid manual payouts. |
| `tblaffiliates.balance` | commission statuses | After the history the newest imported commissions are `available` up to the WHMCS balance, older ones `withdrawn`; the part of the balance the history does not explain becomes one opening-balance commission on the affiliate's first referral, or — without any referral — a `balance_not_imported` warning to settle by hand. |
| `tbltodolist` | `staff_tasks` | Completed → done; assigned to the imported staff member. |
| `tblbannedips`, `tblbannedemails` | `fraud_bans` | IPs, CIDR networks and `a.b.c.*` wildcards (→ network); e-mail domains. Expired bans are skipped; a ban that exists already is adopted. |
| `tbldownloadcats`, `tbldownloads` | `download_categories`, `downloads` | Metadata only: WHMCS keeps the files in its downloads directory, so each file is uploaded again under Admin → Downloads (until then it is listed but not served). Nested categories become top-level ones; product downloads keep the "product" access but their product links must be set again. |
| `tblnetworkissues` | `status_incidents` + one timeline update | Scheduled (or future) issues become maintenance windows. Status: Reported / Investigating → investigating, In Progress / Outage → identified, Resolved → resolved (maintenance: scheduled, in progress, completed). Priority: Critical → critical, High → major, Medium → minor, Low → none. The description, "affecting" text and server name form the update; imported incidents are never announced to subscribers. |

The field names follow the WHMCS API documentation where an API exists
(`GetQuotes`, `GetAffiliates`, `GetToDoItems`); the other tables are read by
their WHMCS 8 column names, and a column that is missing in an older schema
only makes the affected rows skip with a report line.

## The wizard

1. **Source** – database or API.
2. **Connect & test** – the credentials are probed (30 s limit), then sealed
   into the run (see *Secrets*). The page shows the WHMCS version and notes
   (TLS, missing API permissions).
3. **Preview** – a dry run reads everything, writes nothing and reports
   per-entity counts, conflicts (unmapped gateways / registrars, unknown
   server modules, invalid currencies, products that will be created, plan
   limits) and every row it would skip.
4. **Mapping** – gateways and registrars → existing integrations, servers →
   *create as panel X (inactive)* / existing integration / skip, products →
   *create* / existing product, WHMCS admin roles → HostingOS roles. Mapped ids
   must belong to the company.
5. **Import** – needs a finished preview and the explicit confirmation that
   WHMCS automation (cron) has been stopped. Progress updates every 2 s.
6. **Report** – per-entity table, messages filterable by level, CSV / JSON
   download (CSV cells are formula-neutralised), cut-over checklist and
   rollback.

## CLI

```sh
export HOS_IMPORT_DB_PASSWORD=…        # or typed at the prompt
export HOS_IMPORT_CC_HASH=…            # optional: hosting / server passwords
hostingos import whmcs \
  --db-host whmcs-db.example.net --db-name whmcs --db-user reader \
  --dry-run --report preview.csv

hostingos import whmcs --api-url https://billing.example.net/ \
  --api-identifier ID --map gateway:paypal=<integration id> --yes

hostingos import whmcs … --yes --confirm-live   # re-import into a live shop

hostingos import list
hostingos import rollback --run <run id>
```

Secrets are read from `HOS_IMPORT_DB_PASSWORD`, `HOS_IMPORT_CC_HASH`,
`HOS_IMPORT_API_SECRET`, `HOS_IMPORT_API_ACCESS_KEY` or a no-echo prompt —
never from command-line arguments. The CLI runs the preview, prints conflicts
and asks before importing (`--yes` skips the question, `--dry-run` stops
after the preview). CLI runs show up in the web UI, audit entries are written
with actor `system`.

## Passwords

`hos_auth::legacy` stores imported hashes as they are and checks them at
login:

| WHMCS value | Stored as | At login |
|---|---|---|
| `$2y$` / `$2a$` / `$2b$` bcrypt (cost ≤ 14) | unchanged | verified, then transparently re-hashed to argon2id |
| `md5(salt.password):salt` | `$hos-legacy$whmcs-md5$<hex salt>$<md5>` | **no session**: a password-reset link is e-mailed (rate-limited) and the user is told to set a new password |
| bare MD5 | `$hos-legacy$md5$<md5>` | as above |
| anything else / empty | no password | "forgot password" |

Staff passwords are never imported. Hashes are never logged or shown
(`ImportedHash`'s `Debug` omits them). Every login check costs the same (one
argon2id and one bcrypt verification, real or dummy), so timing reveals
neither whether an address exists nor how its password is stored.

## Policies

* **Amount paid.** An open invoice (draft, unpaid, payment pending,
  collections) gets its imported payments minus refunds / chargebacks in the
  invoice currency, clamped to what is due. A paid / refunded invoice keeps
  its WHMCS status. Payments in another currency, overpayments and paid
  invoices whose transactions do not add up are reported, never turned into
  credit (the WHMCS credit balance is imported as is).
* **Staff and roles.** Mapping WHMCS roles needs `staff.manage`; a role can
  only be mapped when the person mapping holds all its permissions; the
  owner role is never mapped. Imported staff start disabled and unverified:
  someone who manages staff activates them, then they set a password with
  "forgot password". The CLI runs as the server operator (only the owner
  rule applies).
* **Logins registered before the import.** An existing client login with
  the same e-mail is linked only when it already belongs to that client or
  its address is verified — then its password is cleared and its sessions
  end. An unverified login is never linked (reported as a conflict).
* **Member permissions.** Only `manageproducts` (services) and
  `managedomains` (domains) grant an area; `products`, `productsso` and
  `domains` (viewing) grant none.
* **Affiliates.** An affiliate account opened in HostingOS is linked to the
  WHMCS affiliate but keeps its own terms, code, currency and money: WHMCS
  commissions, payouts and its balance are not added (the balance is
  reported as `native_affiliate_balance`).
* **Re-import after go-live.** When invoices, payments, orders or credit
  entries were created in HostingOS after the last completed import, a new
  import needs an explicit confirmation (wizard checkbox, CLI
  `--confirm-live`). Credit balances are reconciled over the imported credit
  entries only, so credit used in HostingOS is never added back.

## Secrets

* Source credentials are sealed with the application `SecretBox`
  (AES-256-GCM), AAD `hos:import-source:` + company (tenant) id + run id, and never
  rendered back. They are wiped when an import ends (completed, failed,
  cancelled), on *Wipe credentials*, on cancel of a queued run, and by the
  runner after 24 h at the latest. A finished preview keeps them so the
  import can start without retyping.
* Decrypted service passwords are re-sealed with the billing secret scheme
  (`service_password_aad`), server credentials with the integration scheme.
* The outbound policy (`HOS_OUTBOUND_POLICY`) applies to the source host: the
  database host is resolved once and pinned, the API URL must be `https`
  unless the policy is `unrestricted`. Connect 15 s, query 120 s, API bodies
  are size-capped. A database source cannot exhaust memory: long text / blob
  columns are cut to 256 KiB in the query, rows are streamed and a batch stops
  at 32 MiB, packets are limited to 16 MiB; every row of any source is capped
  the same way and truncated fields are reported. Attachments are capped (default 10 MiB, max 25), their type
  is sniffed and must be on the ticket upload allow-list.
* Imported HTML (product descriptions, KB, announcements, tickets) goes
  through the same sanitizer as staff input; plain-text fields are stored as
  text and escaped when rendered.

## Idempotency, adoption and resume

* Every created row is recorded in `import_id_map`
  (`source_key, entity, source_id → target_id`). Re-running the same source
  updates nothing that was already imported and never duplicates rows.
* Rows that already exist in HostingOS are **adopted** by natural key
  (user e-mail, domain name, invoice / order / ticket number, coupon code,
  currency code, TLD, group / department / role / integration name): they are linked in the id map but never modified and never
  deleted by a rollback.
* Each batch (default 250 rows) is one transaction; on an error the batch is
  replayed row by row so one bad row is reported instead of failing the batch.
  Progress (cursor per entity) is checkpointed with a lease; a run whose
  worker died is picked up again after 5 minutes and continues from the
  last checkpoint. A working run renews its lease every 30 s in the
  background and checks it in every batch transaction: a run whose lease was
  taken over stops instead of writing next to the new worker. *Cancel*
  stops after the current batch.
* Plan limits (`max_clients`, `max_services`, `max_staff`, integrations) are
  checked in the preview and enforced while importing: rows beyond the limit
  are skipped and reported.

## Silent mode and cut-over grace

An import sends no e-mails and triggers no provisioning. In addition:

* **Automation paused** (default on): the company's daily billing run and TLD
  price sync stop until the admin presses *Resume billing automation* on the
  cut-over checklist.
* **Grace days** (default 14): imported services get
  `override_suspend_until = max(WHMCS override, today + grace)`; open
  imported invoices are marked as already late-fee'd, and reminders / domain
  expiry notices that would fall into the window are marked as sent.
* Imported invoice items keep their service period, so the renewal run does
  not invoice an already-invoiced period again; the invoice sequence is moved
  past the highest imported number.
* The checklist shows services still in grace, open invoices, services that
  would be suspended now, and suspended services that resuming automation
  would queue for termination.

## Rollback

*Roll back* (web or CLI) deletes every row the run **created**, children
first, then forgets the id-map entries and restores automation if the run
paused it. It is refused while anything created afterwards references the
imported rows (e.g. a new invoice for an imported client) and lists those
references. Adopted rows are untouched. Audit entries: `import.create`,
`import.preview`, `import.start`, `import.completed`, `import.cancel`,
`import.rollback`, `import.rolled_back`, `import.automation_resumed`.

## Tests

`cargo test -p hos-import` (mappers, hash / decrypt vectors, fixture import
including quotes, affiliates and balances, to-dos, bans, downloads and network
issues, re-run, rollback, dry run, silent mode, company isolation, cancel,
failure, API connector against wiremock) and `cargo test -p hos-web --test it import::`
(permission and CSRF, wizard against a mocked WHMCS API, isolation, bcrypt
login + re-hash, MD5 reset, escaped imported markup).

## Not verified against a real WHMCS

The connectors were built from the WHMCS schema and API documentation and are
tested against fixtures and a mocked API. Before a production cut-over, run a
preview against a copy of the real database and read the report. In
particular: the `decrypt()` port is checked against published community
vectors only; column sets differ between WHMCS versions (missing columns are
tolerated); the API's exact JSON shapes for less common actions; and MySQL 8
`caching_sha2_password` users without TLS (prefer TLS or a
`mysql_native_password` read-only user).
