# HostingOS features

One line per feature area: what it does and where it lives. Details are in
the linked documents.

| Area | What it does | Where |
|---|---|---|
| One hosting company | One installation runs one hosting company: storefront, client area and admin panel on its own address (`HOS_BASE_URL`); `hostingos setup` creates the company and its owner. An advanced multi-company mode (`HOS_MODE=multi`) exists for running several companies on one installation | [DEPLOYMENT.md](DEPLOYMENT.md), [ARCHITECTURE.md](ARCHITECTURE.md) |
| Storefront | Server-rendered Turkish / English shop in seven templates (atlas, classic, cloud, corporate, editorial, studio, terminal) with light / dark mode, product groups, configurable options, currency switcher, bundles, domain search, knowledge base, blog, announcements, downloads, contact form, landing pages | `/`, Admin → Settings → Appearance |
| SEO / GEO | Canonical, hreflang, JSON-LD, sitemap, robots, `llms.txt`, Markdown twins of pages, IndexNow, redirects, meta overrides, site audit, optional AI drafts | Admin → SEO, Admin → AI studio |
| Cart and checkout | Products, add-ons, bundles and domains in one cart; coupons, tax rules, account credit; order → invoice → payment → automatic provisioning; staff accept or cancel pending orders one by one or in bulk | `/cart`, `/checkout` |
| Quotes | Staff quotes for clients or prospects, PDF, public quote link; the client accepts (order + services + invoice in one step) or declines | Admin → Quotes, `/client/quotes`, API |
| Invoices and payments | Recurring invoices, reminders, late fees, refunds, invoice PDF, mass pay of several invoices with one payment, add funds, partial payments, offline (bank transfer) payments recorded by staff; bulk publish / remind / cancel from the invoice list | Admin → Invoices / Transactions, `/client/invoices`, `/client/billing` |
| Payment gateways | Stripe (incl. saved cards and adding a card without paying), iyzico, PayTR, PayPal, Nestpay, Papara, Paynet, bank transfer; installment picker where the gateway offers installments; signed webhooks applied once | Admin → Integrations, [PAYMENTS.md](PAYMENTS.md) |
| Currencies and tax | Several currencies with exchange rates, per-currency prices for products and configurable options, tax rules per country / region, prices shown in the visitor's chosen currency | Admin → Billing |
| E-invoice (Turkey) | e-Fatura / e-Arşiv through Nilvera or Paraşüt, UBL-TR generation, status sync | Admin → e-Invoice, [EINVOICE.md](EINVOICE.md) |
| Hosting provisioning | cPanel / WHM, Plesk, DirectAdmin, CyberPanel: create, suspend, unsuspend, terminate, package change, password reset, single sign-on | Admin → Services, `/client/services` |
| VPS / cloud | Proxmox VE, VMware vSphere (through vCenter Server; standalone ESXi is not supported), Virtualizor, Hetzner Cloud, each action as far as the platform supports it: power, reinstall, snapshots, backups (Proxmox VE and Hetzner Cloud only), console, rescue mode, ISO, firewall, reverse DNS, metrics, bandwidth, SSH keys, IP pools | `/client/services/{id}`, Admin → Services, Admin → IP pools |
| Product add-ons | Add-ons ordered with a product or later for an existing service, invoiced and activated with it | Admin → Catalog → Add-ons, `/client/services/{id}` |
| Domains | Registration, transfer, renewal, nameservers, lock, privacy, EPP code, contacts, DNS; ResellerClub, Namecheap, OpenProvider, RDAP availability; TLD pricing | Admin → Domains / TLDs, `/client/domains` |
| Client area | Dashboard, services, domains, invoices, billing (credit, payment methods), quotes, tickets, downloads, affiliate programme, account (security, 2FA, sessions, sub-users with member areas, API keys, SSH keys) | `/client` |
| Sub-users (member areas) | A client's extra users get only the areas granted to them (services incl. downloads and SSH keys, domains, invoices / billing, orders / quotes, tickets, contacts, profile); sub-users, privacy, API keys and the affiliate account stay with the owner | `/client/account/users` |
| Support | Departments, tickets with attachments, canned replies, internal notes, priorities, bulk actions, knowledge base, announcements | Admin → Support, `/client/tickets` |
| E-mail piping | Tickets opened and answered by e-mail (Mailgun inbound route or raw endpoint), reply tokens, quarantine for unknown senders (release, add to a ticket, discard — also in bulk), loop / flood protection | Admin → Support → Inbound mail, [API.md](API.md) |
| Downloads | Files grouped in categories, public / logged-in / product-owner access, download counts | Admin → Downloads, `/downloads`, `/client/downloads` |
| Bundles | Fixed-price packages of several products sold as one cart line | Admin → Catalog → Bundles, `/bundles` |
| Affiliates | Referral links, click tracking, one-time or recurring commissions with a holding period, payout requests, staff payouts | Admin → Affiliates, `/client/affiliate`, API |
| Network status | Components, incidents and maintenance windows with timelines, e-mail notification of an incident, public status page and JSON | Admin → Status, `/status`, `/api/v1/status` |
| Staff and permissions | Staff users, roles with fine-grained permissions, 2FA enforcement, notification preferences, audit log, impersonation of clients (audited, ended before admin pages are shown again) | Admin → Staff, Admin → System |
| Staff tasks | To-do list with assignee, client, priority and due date; bulk complete / reopen | Admin → Tasks, `/api/v1/tasks` |
| Fraud protection | Banned IPs / networks / e-mail domains, order screening | Admin → Fraud |
| Reports and dashboard | Revenue (by product, group, gateway), MRR / churn growth, tax, receivables aging, best sellers, domain renewals, CSV export, global search | Admin → Dashboard / Reports |
| E-mail | SMTP, Resend, Postmark, Mailgun; editable Turkish / English templates (a copy of the default is not marked as customized), mail queue | Admin → Settings → E-mail |
| REST API | `/api/v1` with API keys (staff or client), pagination, problem+json errors, idempotency keys, rate limits, OpenAPI 3.1 and a reference page; outgoing signed webhooks | [API.md](API.md), `/api/docs` |
| WHMCS import | Clients, products, services, domains, invoices, tickets, quotes, affiliates, to-dos, bans, downloads, network issues and more, from the database or the API, with dry run and rollback | Admin → Tools → Import, [IMPORT-WHMCS.md](IMPORT-WHMCS.md) |
| Background jobs | PostgreSQL job queue with cron schedules (invoicing, reminders, suspensions, domain sync, mail), in the web process or a separate worker | `hostingos worker`, Admin → System → Jobs |
| Server pages | For the owner only (`system.manage`): version, uptime, database, migrations and health | Admin → Server (`/admin/server`) |
| Updates | Signed (minisign) release check, apply with database backup (the owner confirms with password and 2FA), migrations and maintenance mode, deploy webhook for containers, discreet "update available" indicator for the owner in the admin | Admin → Server → Updates (`/admin/server/updates`), [UPDATES.md](UPDATES.md) |
| Security | argon2id passwords, CSRF, strict CSP, TOTP 2FA, OAuth sign-in, session management, rate limits, encrypted integration secrets, outbound HTTP through guarded clients | [../SECURITY.md](../SECURITY.md) |
| Deployment | One binary plus PostgreSQL; Docker Compose (any container platform), systemd unit, Caddy with automatic TLS, `hostingos healthcheck` for container health checks | [DEPLOYMENT.md](DEPLOYMENT.md) |
| Public demo | Demo mode with published logins, mock integrations, no outbound traffic and a full reset every 30 minutes | [DEMO.md](DEMO.md) |
