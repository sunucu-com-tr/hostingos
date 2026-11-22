# Payments

HostingOS takes payments through **gateway integrations** (Admin → Integrations →
Add → category *Gateway*). Every gateway is configured from a descriptor-driven
form: config fields are stored in plain JSON, **secret fields are encrypted**
(AES-256-GCM, bound to the company and integration) and are never shown again —
leaving a secret field empty on edit keeps the stored value.

Card data never touches HostingOS: every card gateway uses the provider's
hosted page, iframe, embedded hosted form or the bank's 3-D "pay hosting" page
(PCI DSS SAQ-A).

## Callback URL

Every gateway uses one URL per integration, shown on the integration's edit page:

```
https://<your-site>/webhooks/gateway/<integration_id>
```

It accepts `GET` and `POST` (provider webhooks, server-to-server callbacks and
browser returns). Where the provider has a dashboard setting for it, paste it
there (table below); the others receive it with every payment.

## Gateways

| Kind | Provider | Customer sees | Callback / confirmation | Set the URL in the provider panel? |
|---|---|---|---|---|
| `stripe` | Stripe | Stripe Checkout (redirect); cards, Apple Pay, Google Pay, Link and other methods enabled in the Stripe Dashboard | `Stripe-Signature` (HMAC-SHA256, ±5 min) | **Yes** — webhook endpoint with `checkout.session.completed`, `checkout.session.async_payment_succeeded`, `checkout.session.async_payment_failed`, `payment_intent.payment_failed`, `payment_intent.succeeded`, `charge.refunded`, `charge.dispute.created` |
| `paypal` | PayPal (Orders v2) | PayPal (redirect), captured on return | PayPal `verify-webhook-signature`; the return captures server-side | **Yes** — webhook with `PAYMENT.CAPTURE.COMPLETED/PENDING/DENIED/REFUNDED`, `CUSTOMER.DISPUTE.CREATED`; paste the webhook id |
| `iyzico` | iyzico | Checkout Form (redirect or embedded) | callback token → server-side retrieve; `X-IYZ-SIGNATURE-V3` | Optional (merchant notification URL) |
| `paytr` | PayTR | iFrame | notification hash (HMAC-SHA256), reply `OK` | **Yes** — "Bildirim URL" |
| `nestpay` | NestPay / Payten bank virtual POS (Ziraat, İş Bankası, Halkbank, TEB, Şekerbank, other NestPay banks) | the bank's 3D Pay Hosting card page | hash ver3 (Base64 SHA-512) over the bank's result; server callback answered `Approved` | No — `okUrl`/`failUrl`/`callbackUrl` are sent with each payment |
| `paynet` | Paynet | Paynet.js hosted card form embedded in the pay page | server-side `v1/transaction/charge` with the secret key; amount and reference signed into the form | No (Paynet requires your server's fixed IP to be registered) |
| `papara` | Papara Checkout | Papara (redirect) | IPN `merchantSecretKey` + `GET /payments?id=` | No — `notificationUrl` is sent with each payment |
| `bank_transfer` | Manual EFT / wire | instructions | staff confirms the payment | — |

## Features

| | Stripe | PayPal | iyzico | PayTR | NestPay | Paynet | Papara | Bank transfer |
|---|---|---|---|---|---|---|---|---|
| Currencies | all Stripe currencies | 25 PayPal currencies (no TRY) | TRY, USD, EUR, GBP, NOK, CHF | TRY, USD, EUR, GBP, RUB | TRY, USD, EUR, GBP (as enabled by the bank) | TRY | TRY, USD, EUR | configured accounts |
| Refund from HostingOS | ✓ full / partial | ✓ full / partial | ✓ | ✓ | — (bank panel) | ✓ (request, completes via signed `succeedUrl`) | ✓ full / partial | — |
| Refund id recorded | ✓ | ✓ | ✓ | ✓ (reference) | — | ✓ (when Paynet sends it) | — | — |
| Disputes / chargebacks | ✓ `charge.dispute.created` | ✓ `CUSTOMER.DISPUTE.CREATED` | — | — | — | — | — | — |
| Saved method for auto-renewal | ✓ card (off-session PaymentIntent) | ✓ PayPal vault (opt-in, needs PayPal approval) | — | — | — | — | — | — |
| Installments | — | — | ✓ | ✓ | ✓ (TRY, 2 to `max_installment`, chosen by the client on the pay page) | ✓ (installment codes) | — | — |
| Installment interest | — | — | never credited | never credited | n/a | never credited (commission option) | — | — |
| Connection test | ✓ | ✓ | ✓ | — | gate reachability only | ✓ | ✓ | config check |
| Exact attempt match | — | — | — | — | ✓ | ✓ | ✓ | — |

**Exact attempt match**: the provider's signed / API-confirmed amount must equal
the pending payment attempt (same integration, reference, amount, currency).
Anything else is rejected, stored for review and staff are notified
(`payment.attempt_mismatch`) instead of being booked as a partial payment or
credit. Installment interest or a commission paid by the customer is recorded
on the transaction for information and never becomes client credit.

Replays are idempotent: every event has a stable id (`nestpay:{oid}:paid`,
`paynet:{reference}:paid`, `papara:{id}:paid`, Stripe `evt_…`, …) and is
applied once; a callback for one integration is never accepted on another (a
different key fails verification, and a payment reference belongs to the
integration that started it).

## Setup

### Stripe
1. Dashboard → Developers → API keys: secret key `sk_live_…` (or a restricted `rk_` key).
2. Developers → Webhooks → add the callback URL with the events listed above; copy the signing secret `whsec_…`.
3. Payment methods: enable cards, Apple Pay (verify your domain), Google Pay, Link — Checkout shows them automatically.
4. **Rotating the webhook secret**: in Stripe "Roll secret" with a delay (up to 7 days), enter the new secret as *Webhook signing secret* and the old one as *Previous webhook signing secret*; clear the previous one after the delay.
5. Saved cards: the client ticks "save my card" on the pay page; renewals are charged off-session on the due date (Admin → Settings → *Charge stored cards automatically*).

### PayPal
1. developer.paypal.com → Apps & Credentials: client id and secret (sandbox or live).
2. Add a webhook to the callback URL with the events above and paste its id.
3. Vault (optional): ask PayPal to enable vaulting / reference transactions on the account, then tick *Save PayPal accounts for renewals*.

### NestPay / Payten (bank 3D Pay Hosting)
1. Ask the bank for a **3D_PAY_HOSTING** virtual POS with **hash ver3** and the merchant panel login.
2. In the bank's merchant panel set the **store key** (*İşyeri güvenlik anahtarı*).
3. In HostingOS choose the bank, enter the *clientid* and the store key. Test mode uses Payten's integration gate (`entegrasyon.asseco-see.com.tr`). For live, Ziraat's gate is built in; for other banks paste the live 3D gate URL from the bank's welcome pack (it must be on the bank's own domain and end in `/fim/est3Dgate`).
4. Refunds and cancellations are made in the bank panel; record them in HostingOS as a manual refund.

### Paynet
1. Request API keys from Paynet (publishable `pbk_…` and secret `sck_…`) and register your server's fixed IP with Paynet.
2. Enter both keys; optionally the allowed installment codes (e.g. `0,3,6,9`) and whether the customer pays the commission.
3. Refunds started in HostingOS stay *pending* until Paynet confirms them on the signed callback URL (sent with the request).

### Papara
1. Papara merchant panel: API key and secret key (test and live keys differ).
2. Keep **secret key hashing disabled** (its exact format is not documented, so HostingOS only accepts the plain secret).
3. Optional: restrict API access to your server IP in the Papara panel.

## Not implemented

Turkish providers considered and left out because their official documentation
was not publicly reachable or did not specify callback verification precisely
enough (a wrong payment integration is worse than a missing one):

| Provider | Reason |
|---|---|
| Craftgate | developer documentation behind a merchant login |
| Param (TURKPOS) | developer site refused automated access (HTTP 403) |
| Sipay | request hash only produced by an interactive generator; documented response validation is unauthenticated AES-CBC |
| Garanti BBVA (GVP / OOS) | response authentication and order-inquiry statuses not documented |
| Moka United | result hash needs a per-attempt value; deferred |
| Akbank (new virtual POS API) | JavaScript-only portal, content not retrievable |
| Yapı Kredi POSNET, Kuveyt Türk, VakıfBank | card data posted through the merchant server, or documents not public |
| QNB PayFor | documentation behind a login |
| Lidio, Hepsipay, Tami | documentation not reachable |
| Shopier | public API is shop management, not payment acceptance |
| PayU Türkiye | merged into iyzico |

## Refunds and chargebacks of credit payments

A combined ("mass pay") invoice and an add-funds invoice buy client credit,
which is then applied to invoices. Taking that money back reaches those
invoices:

* **Refund** of a combined invoice: the credit it applied is taken back from
  its invoices, newest first, up to the refunded amount; those invoices
  become refunded / partially refunded. A partial refund undoes only that
  much. The combined invoice itself is closed as refunded.
* **Chargeback** of a combined invoice: the invoices it paid are reopened
  (owed again) and their services follow the chargeback setting
  (`billing.suspend_on_chargeback`); affiliate commissions on them are
  reversed by the daily maintenance (one already paid out as a negative
  adjustment, once). The combined invoice is closed, not reopened, so the
  client owes the money once.
* **Chargeback of credit that was spent elsewhere** (add funds, or credit
  that cannot be traced to an invoice): the client's balance goes negative
  by that amount — a negative balance cannot be spent — and staff are
  notified (`payment.chargeback_credit_shortfall`). A staff refund needs the
  balance plus the traceable applications to cover it.
* An invoice of a combined payment paid some other way meanwhile: the
  untouched combined invoice is cancelled, and paying it is refused (select
  the open invoices again). If money still arrives for it, only what is still
  owed is applied; credit left over above the add-funds limits stays on the
  account and staff are notified (`payment.mass_pay_excess_credit`) to refund
  it.

## Stored callbacks

Callbacks that pass verification are stored with card fields (PAN, expiry,
CVV, NestPay 3-D Secure `md` / `cavv` / `xid` / `eci`, card tokens, …)
removed; failed ones only as size and SHA-256 hash.
`hostingos webhooks scrub` removes card fields from rows stored earlier.

## Verification status

Every gateway in this document is verified **only against the provider's
published documentation and local mock servers** (wiremock / signed fixtures),
not against the providers' live or sandbox systems. Run a sandbox payment,
refund and webhook delivery for each gateway before going live.

## Known gaps

* Saved cards are not implemented for iyzico (card storage through the
  Checkout Form must first be enabled by iyzico, which returns `cardUserKey` /
  `cardToken` only then — the public docs ask merchants to contact them),
  PayTR (its card storage API was not reviewed) and Paynet. Stripe is the
  only gateway where a client can add a card without paying (a Checkout
  session in setup mode, from `/client/billing`).
* PayPal: if PayPal's capture webhook is processed before the buyer's return,
  the vault token of that payment is not stored (the return is then a replay).
