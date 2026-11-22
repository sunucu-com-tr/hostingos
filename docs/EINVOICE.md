# e-Fatura / e-Arşiv and invoice PDFs

HostingOS can issue its invoices as Turkish legal e-documents (e-Fatura or
e-Arşiv Fatura) through a private integrator (*özel entegratör*), and it
renders every invoice as a real PDF. This page describes what is
implemented, how it behaves legally, what is left to staff, and what has
only been verified against documentation and mock servers.

| Piece | Where |
|---|---|
| Provider contract (`EInvoiceProvider`, category `einvoice`) | `crates/hos-core/src/integrations/einvoice.rs` |
| Providers, UBL-TR builder, VKN / TCKN, GİB code lists | `crates/hos-einvoice` |
| Automation (hooks, jobs, sweep, staff actions) | `crates/hos-billing/src/einvoice.rs` |
| Invoice PDF (data → layout) | `crates/hos-billing/src/invoice_pdf.rs`, `crates/hos-pdf` |
| Storage | `crates/hos-db/migrations/0034_einvoice.sql`, `crates/hos-db/src/repo/einvoice.rs` |
| Admin / client / API | `crates/hos-web/src/routes/admin/einvoice.rs`, `…/admin/settings/einvoice.rs`, `…/client/invoice_files.rs`, `…/api/documents.rs`, `crates/hos-web/src/einvoice.rs` |
| Translations | `locales/{tr,en}/einvoice.json` |

## Providers

Every provider module starts with a header that records the documentation
it was written from, the date it was read, the API version, the auth scheme
and the test host. Nothing was written from memory.

| Kind | Status | Docs (read 2026-11-18) | API / auth | Test host | Numbering |
|---|---|---|---|---|---|
| `parasut` | implemented | Paraşüt API v4 reference (apidocs.parasut.com, via the official swagger-generated OpenAPI artifacts) and the `parasutcom/api-doc` repository | v4 JSON:API; OAuth 2 password grant, refresh token | none documented — base URL configurable (test companies on request) | Paraşüt assigns number and ETTN |
| `nilvera` | implemented | developer.nilvera.com (environments, API keys, error codes, General / e-Fatura / e-Arşiv APIs with embedded OpenAPI 3.0.1) | v1 REST; `Authorization: Bearer <API key>`; UBL-TR XML upload | `https://apitest.nilvera.com` (live `https://api.nilvera.com`) | HostingOS assigns number and ETTN |
| `mock` | implemented (opt-in) | — | in memory | — | HostingOS |

Capabilities differ and the automation reads them from the provider:

| | Paraşüt | Nilvera |
|---|---|---|
| Taxpayer (e-Fatura user) lookup | yes | yes |
| e-Arşiv / e-Fatura (TEMELFATURA) | yes | yes |
| Legal PDF | yes | yes |
| UBL download | no (not documented) | yes |
| e-Arşiv cancellation | no (not documented) → staff | yes |
| Return invoice (IADE) | no | yes (UBL), not issued automatically |
| Foreign currency | yes | yes |

**Evaluated and not implemented**

| Integrator | Reason |
|---|---|
| Uyumsoft | not implemented: only a WSDL is public; numbering, duplicate handling and cancellation semantics are undocumented |
| EDM | not implemented: only a WSDL is public, same gaps |
| İzibiz | not implemented: public developer material does not specify the invoice-creation contract |
| KolayBi | not implemented: public API documentation lacks the e-document contract details |
| Trendyol E-Faturam, BizimHesap | not implemented: docs unavailable (documentation URLs answered 404) |
| Kolaysoft | not implemented: docs unavailable (timed out) |
| Sovos, Logo İşbaşı, QNB eFinans | not implemented: docs unavailable (hosts did not resolve from the build environment) |
| Turkcell e-Şirket, NES, Digital Planet, İdea, Mikro / Zirve, Logo eLogo | not evaluated |

Adding a provider: implement `EInvoiceProvider` in a new `hos-einvoice`
module, describe it in `descriptors()`, register the kind in `build()`, add
a wiremock suite like `crates/hos-einvoice/tests/nilvera.rs`.

## Legal behaviour

* **e-Fatura or e-Arşiv.** A buyer with a real VKN / TCKN is looked up in the
  GİB e-Fatura user list through the provider. Registered users get an
  e-Fatura (profile `TEMELFATURA`) to their first active alias; everybody
  else an e-Arşiv invoice (`EARSIVFATURA`). Consumers without a TCKN get
  the placeholder `11111111111`, buyers outside Türkiye `2222222222`;
  neither is looked up and neither is ever accepted from a form.
* **Parties.** Companies: VKN + tax office + title. Individuals (and sole
  proprietors with a TCKN): first and family name. The seller comes from
  *Settings → General* (legal name, VKN / TCKN, tax office, address).
* **Lines and totals.** `hos_einvoice::compose` turns the HostingOS invoice
  into UBL lines: discounts and credit lines become line allowances,
  the invoice's own VAT total is distributed over the lines (largest
  remainder), 0 % lines carry the configured exemption code and reason.
  The UBL payable amount always equals the HostingOS total to the kuruş;
  Paraşüt's computed `net_total` is compared too and a mismatching draft is
  deleted before anything legal exists.
* **Codes.** Exemption codes are checked against the GİB list: `ISTISNA`
  invoice type for exemption codes other than 351 / 555, `SATIS` otherwise;
  special-base (8xx) and export-registered (7xx) invoices are refused.
  Withholding (`TEVKIFAT`) is in the contract and the UBL builder (code +
  rate validated) but HostingOS invoices never produce it.
* **Currency.** Non-TRY invoices carry `PricingExchangeRate` (TRY per unit,
  from the currency table, ≤ 6 decimals).
* **Internet sale.** e-Arşiv documents of online payments carry the web
  address, payment method, payment date and intermediary (gateway).
* **Dates.** Issue date in Türkiye time (UTC+3), never in the future.
* **Numbering** (Nilvera): `SSSYYYYNNNNNNNNN` — series (3 characters, one for
  e-Fatura, one for e-Arşiv) + year + 9-digit sequence, allocated once per
  document inside a transaction (`einvoice_sequences`), kept on retries.
* **Exactly one document per invoice.** `einvoice_documents` is unique per
  invoice and purpose; the worker leases the row before calling the
  provider; the ETTN, number and request are fixed on the first attempt;
  Nilvera answers a repeated ETTN with 409 / code 1001, which is treated as
  "already submitted"; Paraşüt resumes from its stored checkpoint.
* **Failures never block payments.** The payment hook runs in its own
  savepoint. Provider errors are retried by the job queue (8 attempts);
  validation or provider refusals mark the document `failed` and notify
  staff; transient errors on the last attempt notify staff too.
* **Refunds, cancellations, chargebacks** (10-minute sweep):
  * full refund or cancellation of an invoice with an e-Arşiv still inside
    its cancellation window → cancelled at the provider (Nilvera);
  * an e-Fatura cannot be cancelled by the seller → `attention` + staff
    notification ("agree a return invoice with the buyer");
  * partial refunds and chargebacks → `attention` + notification;
  * provider without cancellation (Paraşüt) or a failed cancellation →
    `attention` + notification.
* **Reconciliation.** Processing documents are polled; issued documents are
  re-read once a day for 45 days (buyer rejection of a commercial invoice,
  cancellation made in the provider's own panel).
* **Not issued.** Add-funds (credit top-up) invoices, zero-total invoices,
  invoices issued before `einvoice.start_date`, drafts and cancelled
  invoices. Invoices mixing credit lines with sales fail for staff.

**Left to staff:** return invoices (IADE) for e-Fatura buyers and partial
refunds, credit notes, chargebacks, responding to buyer rejections, and
anything flagged on the *e-Documents* page.

## Settings

*Settings → e-Invoice* (`settings.manage`):

| Key | Default | |
|---|---|---|
| `einvoice.enabled` | off | master switch; also turns on VKN / TCKN validation for clients in Türkiye |
| `einvoice.integration_id` | default `einvoice` integration | which integrator |
| `einvoice.issue_on` | `payment` | `payment` or `creation` (sweep) |
| `einvoice.start_date` | the day it is switched on | invoices before it are never e-invoiced |
| `einvoice.test_mode` | off | only issue through a test-environment integration |
| `einvoice.series_earsiv` / `einvoice.series_efatura` | — | required while on; must differ |
| `einvoice.exemption_code` / `…_reason` | — | for 0 % VAT lines |
| `invoice_pdf.attach_to_emails` | off | attach the PDF to invoice e-mails |
| `invoice_pdf.bank_details` | — | printed at the bottom of the PDF |

Connection details live on the integration (*Integrations → e-Invoice*):
credentials (Paraşüt client secret and password, Nilvera API key) are
integration **secrets**, encrypted at rest and never echoed back.

## Screens and endpoints

| | Permission | |
|---|---|---|
| `/admin/einvoice` | `billing.view` | documents; filter *problems* (failed, rejected, needs staff) / all / status |
| Admin invoice page card | `billing.view` / `billing.manage` | status, number, ETTN, errors; issue now, retry, refresh, cancel e-Arşiv, dismiss warning, legal PDF / UBL |
| `/admin/invoices/{id}/pdf` | `billing.view` | invoice PDF |
| `/client/invoices/{id}/pdf` | own invoice, `invoices` member area | invoice PDF |
| `/client/invoices/{id}/e-invoice` | own invoice | legal PDF of an issued, non-test document |
| `GET /api/v1/invoices/{id}/pdf` | `billing.view`; client keys: own invoices | invoice PDF (`?locale=tr|en`), in the OpenAPI document |

Client forms (profile, checkout) and the admin client form check VKN /
TCKN check digits — and require a valid tax number plus tax office for
companies — only while e-invoicing is on and the client's country is TR.

## Security

* Every query is scoped to the company (`tenant_id`); documents of another
  client answer 404. Downloads check ownership (client) or permission (staff).
* POSTs pass the CSRF middleware and write `audit_log` (`einvoice.*`,
  `settings.einvoice`).
* Outbound calls use the `hos_core::net` guarded client: no redirects,
  timeouts, response body caps (4 MiB API, 12 MiB documents), outbound
  policy (private addresses refused in production).
* Logs and stored errors are scrubbed: secrets and 10 / 11-digit numbers
  (VKN / TCKN) are masked.
* XML is written by an escaping writer (no string concatenation of client
  data); provider XML is parsed with DOCTYPE / entities refused and size
  and depth limits.
* Downloads use `Content-Disposition: attachment` with an ASCII-only file
  name, `nosniff`, `no-store` and a sandbox CSP. Bodies are bounded and
  sent in one piece with their length (not streamed from the provider: the
  provider body is size-capped and checked first).

## Invoice PDF

`hos-pdf` renders invoices in pure Rust with
[krilla](https://github.com/LaurenzV/krilla) 0.8: Geist (SIL OFL 1.1) with
Inter's latin-ext subset as fallback for `₺`, subset-embedded with
`ToUnicode`, so text is selectable and extracts with correct ç ğ ı İ ö ş ü
and ₺. Contents: logo (or brand name), seller and buyer blocks (billing
snapshot), the legal e-document number / ETTN when issued, item table
(continues over pages with repeated header), VAT breakdown, totals, payment
stamp (paid, unpaid, overdue, refunded, partially refunded, cancelled, draft), notes, bank details,
`Sayfa n / m` / `Page n / m`; Turkish or English. Money is formatted by the
same rules as the HTML invoice (a test compares both). All strings are drawn
as glyphs after control and bidi-override characters are removed.

## Schema (migration 0034)

* `integrations.category` accepts `einvoice`;
* `einvoice_documents` — one row per invoice and purpose (`sale` /
  `return`): kind, profile, type code, ETTN, legal number, series, status
  (`pending`, `processing`, `issued`, `failed`, `rejected`, `cancelled`,
  `skipped`), provider id, checkpoint, frozen request, test flag, attempts,
  last error, provider message, staff attention, lease and timestamps;
  composite tenant foreign keys;
* `einvoice_sequences` — gap-free numbers per company, series and year.

## Verification status

The Paraşüt and Nilvera clients were written from their public
documentation and are tested against **wiremock servers that replay the
documented requests and responses** (`crates/hos-einvoice/tests`). The
UBL-TR output is checked by unit tests against the GİB rules listed above
(an offline subset of the schematron), not by GİB's validator. The billing
automation is tested end to end with the mock provider on a real database.
**None of this has been run against a live integrator account or the GİB
test environment.** Before going live, issue test documents in each
integrator's test environment (Nilvera: `apitest.nilvera.com`; Paraşüt: a
test company) with `einvoice.test_mode` on, and compare the legal PDFs with
the HostingOS invoices.
