//! OpenAPI 3.1 description of `/api/v1` ([`document`]) and the
//! self-contained HTML reference at `/api/docs` (server-rendered, no CDN, no
//! scripts). [`OPERATIONS`] must list exactly the routes of
//! [`super::route_table`] — a test enforces it.

use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::state::AppState;
use crate::view::escape_html;

/// Who may call an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// No authentication.
    Public,
    /// Any authenticated key / session.
    Any,
    /// Staff with the permission; client keys restricted to own records.
    Scoped(&'static str),
    /// Staff with the permission only.
    Staff(&'static str),
}

/// One documented operation.
#[derive(Debug, Clone, Copy)]
pub struct Operation {
    /// HTTP method.
    pub method: &'static str,
    /// Path template (`{id}` params).
    pub path: &'static str,
    /// Tag (group).
    pub tag: &'static str,
    /// One-line summary.
    pub summary: &'static str,
    /// Access rule.
    pub access: Access,
    /// Request body schema (component name).
    pub request: Option<&'static str>,
    /// Success status.
    pub status: u16,
    /// Response schema (component name); `List:<Name>` for paginated lists,
    /// `Collection:<Name>` for `{total, data}`, `Binary:<media type>` for a
    /// file download.
    pub response: &'static str,
    /// Accepts the standard list query parameters.
    pub list: bool,
    /// Extra query parameters `(name, description)`.
    pub query: &'static [(&'static str, &'static str)],
}

const fn op(
    method: &'static str,
    path: &'static str,
    tag: &'static str,
    summary: &'static str,
    access: Access,
    response: &'static str,
) -> Operation {
    Operation {
        method,
        path,
        tag,
        summary,
        access,
        request: None,
        status: 200,
        response,
        list: false,
        query: &[],
    }
}

const fn with_body(mut o: Operation, request: &'static str, status: u16) -> Operation {
    o.request = Some(request);
    o.status = status;
    o
}

const fn listed(mut o: Operation) -> Operation {
    o.list = true;
    o
}

const fn with_query(mut o: Operation, q: &'static [(&'static str, &'static str)]) -> Operation {
    o.query = q;
    o
}

const fn status(mut o: Operation, s: u16) -> Operation {
    o.status = s;
    o
}

use Access::{Any, Public, Scoped, Staff};

const CURRENCY_Q: &[(&str, &str)] =
    &[("currency", "ISO 4217 code; defaults to the tenant currency")];

/// Every API operation.
pub const OPERATIONS: &[Operation] = &[
    op(
        "GET",
        "/api/v1/ping",
        "Meta",
        "Liveness check of the tenant API",
        Public,
        "Ping",
    ),
    op(
        "GET",
        "/api/v1/me",
        "Meta",
        "The calling principal and its permissions",
        Any,
        "Me",
    ),
    op(
        "GET",
        "/api/v1/stats/dashboard",
        "Meta",
        "Dashboard figures",
        Staff("reports.view"),
        "Object",
    ),
    op(
        "GET",
        "/api/v1/integrations",
        "Integrations",
        "Configured integrations (metadata only, never secrets)",
        Staff("integrations.manage"),
        "Collection:Integration",
    ),
    op(
        "GET",
        "/api/v1/webhook-endpoints",
        "Webhooks",
        "Outgoing webhook endpoints",
        Staff("integrations.manage"),
        "Collection:WebhookEndpoint",
    ),
    with_body(
        op(
            "POST",
            "/api/v1/webhook-endpoints",
            "Webhooks",
            "Create an endpoint; the signing secret is returned once",
            Staff("integrations.manage"),
            "WebhookEndpointCreated",
        ),
        "WebhookEndpointInput",
        201,
    ),
    op(
        "DELETE",
        "/api/v1/webhook-endpoints/{id}",
        "Webhooks",
        "Delete an endpoint",
        Staff("integrations.manage"),
        "Deleted",
    ),
    with_query(
        op(
            "GET",
            "/api/v1/catalog",
            "Catalog",
            "Public product catalog with prices",
            Public,
            "Catalog",
        ),
        CURRENCY_Q,
    ),
    with_query(
        op(
            "GET",
            "/api/v1/tlds",
            "Catalog",
            "TLDs with register / transfer / renew prices",
            Public,
            "Collection:Tld",
        ),
        CURRENCY_Q,
    ),
    with_query(
        listed(op(
            "GET",
            "/api/v1/products",
            "Catalog",
            "Products incl. hidden ones",
            Staff("products.manage"),
            "List:Product",
        )),
        &[("retired", "`1` includes retired products")],
    ),
    op(
        "GET",
        "/api/v1/products/{id}",
        "Catalog",
        "A product with all prices",
        Staff("products.manage"),
        "Product",
    ),
    listed(op(
        "GET",
        "/api/v1/clients",
        "Clients",
        "Clients (a client key sees itself)",
        Scoped("clients.view"),
        "List:Client",
    )),
    with_body(
        op(
            "POST",
            "/api/v1/clients",
            "Clients",
            "Create a client (counts against the plan limit)",
            Staff("clients.manage"),
            "Client",
        ),
        "ClientInput",
        201,
    ),
    op(
        "GET",
        "/api/v1/clients/{id}",
        "Clients",
        "A client",
        Scoped("clients.view"),
        "Client",
    ),
    with_body(
        op(
            "PATCH",
            "/api/v1/clients/{id}",
            "Clients",
            "Update a client (partial)",
            Staff("clients.manage"),
            "Client",
        ),
        "ClientInput",
        200,
    ),
    listed(op(
        "GET",
        "/api/v1/orders",
        "Orders",
        "Orders",
        Scoped("billing.view"),
        "List:Order",
    )),
    with_body(
        op(
            "POST",
            "/api/v1/orders",
            "Orders",
            "Place an order: items → quote → checkout (invoice, services)",
            Scoped("billing.manage"),
            "OrderCreated",
        ),
        "OrderInput",
        201,
    ),
    with_body(
        op(
            "POST",
            "/api/v1/orders/quote",
            "Orders",
            "Price items without ordering",
            Scoped("billing.manage"),
            "Quote",
        ),
        "OrderInput",
        200,
    ),
    op(
        "GET",
        "/api/v1/orders/{id}",
        "Orders",
        "An order with its items",
        Scoped("billing.view"),
        "Order",
    ),
    with_query(
        op(
            "GET",
            "/api/v1/quotes",
            "Quotes",
            "Sales quotes (newest 100; client keys: their sent quotes)",
            Scoped("quotes.view"),
            "Collection:SalesQuote",
        ),
        &[(
            "status",
            "draft, sent, accepted, declined, expired or cancelled",
        )],
    ),
    op(
        "GET",
        "/api/v1/quotes/{id}",
        "Quotes",
        "A sales quote with its lines",
        Scoped("quotes.view"),
        "SalesQuote",
    ),
    status(
        op(
            "POST",
            "/api/v1/quotes/{id}/accept",
            "Quotes",
            "Accept a sent quote: creates the order and invoice (staff also need billing.manage)",
            Scoped("quotes.manage"),
            "QuoteAcceptance",
        ),
        201,
    ),
    with_body(
        op(
            "POST",
            "/api/v1/quotes/{id}/decline",
            "Quotes",
            "Decline a sent quote",
            Scoped("quotes.manage"),
            "SalesQuote",
        ),
        "QuoteDecline",
        200,
    ),
    listed(op(
        "GET",
        "/api/v1/invoices",
        "Invoices",
        "Invoices",
        Scoped("billing.view"),
        "List:Invoice",
    )),
    op(
        "GET",
        "/api/v1/invoices/{id}",
        "Invoices",
        "An invoice with items and transactions",
        Scoped("billing.view"),
        "Invoice",
    ),
    with_query(
        op(
            "GET",
            "/api/v1/invoices/{id}/pdf",
            "Invoices",
            "The invoice as a PDF document (attachment)",
            Scoped("billing.view"),
            "Binary:application/pdf",
        ),
        &[("locale", "tr or en; defaults to the client's language")],
    ),
    with_body(
        op(
            "POST",
            "/api/v1/invoices/{id}/pay",
            "Invoices",
            "Start a gateway payment; returns how to continue",
            Scoped("billing.manage"),
            "PaymentSession",
        ),
        "PayInput",
        200,
    ),
    with_body(
        op(
            "POST",
            "/api/v1/invoices/{id}/payments",
            "Invoices",
            "Record a manual (offline) payment",
            Staff("billing.manage"),
            "ManualPaymentResult",
        ),
        "ManualPaymentInput",
        201,
    ),
    listed(op(
        "GET",
        "/api/v1/transactions",
        "Invoices",
        "Payment transactions",
        Scoped("billing.view"),
        "List:Transaction",
    )),
    listed(op(
        "GET",
        "/api/v1/services",
        "Services",
        "Services",
        Scoped("services.view"),
        "List:Service",
    )),
    op(
        "GET",
        "/api/v1/services/{id}",
        "Services",
        "A service",
        Scoped("services.view"),
        "Service",
    ),
    status(
        with_body(
            op(
                "POST",
                "/api/v1/services/{id}/suspend",
                "Services",
                "Queue a suspension",
                Staff("services.manage"),
                "Queued",
            ),
            "ReasonInput",
            202,
        ),
        202,
    ),
    status(
        op(
            "POST",
            "/api/v1/services/{id}/unsuspend",
            "Services",
            "Queue an unsuspension",
            Staff("services.manage"),
            "Queued",
        ),
        202,
    ),
    status(
        op(
            "POST",
            "/api/v1/services/{id}/terminate",
            "Services",
            "Queue a termination",
            Staff("services.manage"),
            "Queued",
        ),
        202,
    ),
    with_body(
        op(
            "POST",
            "/api/v1/services/{id}/power",
            "Services",
            "VM power action",
            Scoped("services.manage"),
            "ActionResult",
        ),
        "PowerInput",
        200,
    ),
    with_body(
        op(
            "POST",
            "/api/v1/services/{id}/reinstall",
            "Services",
            "Reinstall a VM from a template",
            Scoped("services.manage"),
            "ActionResult",
        ),
        "ReinstallInput",
        202,
    ),
    op(
        "GET",
        "/api/v1/services/{id}/snapshots",
        "Services",
        "VM snapshots",
        Scoped("services.view"),
        "Collection:Snapshot",
    ),
    with_body(
        op(
            "POST",
            "/api/v1/services/{id}/snapshots",
            "Services",
            "Create a snapshot",
            Scoped("services.manage"),
            "ActionResult",
        ),
        "SnapshotInput",
        201,
    ),
    op(
        "POST",
        "/api/v1/services/{id}/snapshots/{snapshot}/rollback",
        "Services",
        "Roll back to a snapshot",
        Scoped("services.manage"),
        "ActionResult",
    ),
    op(
        "DELETE",
        "/api/v1/services/{id}/snapshots/{snapshot}",
        "Services",
        "Delete a snapshot",
        Scoped("services.manage"),
        "ActionResult",
    ),
    op(
        "POST",
        "/api/v1/services/{id}/console",
        "Services",
        "Short-lived console access",
        Scoped("services.manage"),
        "Console",
    ),
    with_body(
        op(
            "POST",
            "/api/v1/services/{id}/password-reset",
            "Services",
            "Reset the VM root password (returned once, not stored)",
            Scoped("services.manage"),
            "PasswordReset",
        ),
        "PasswordResetInput",
        200,
    ),
    op(
        "GET",
        "/api/v1/services/{id}/backups",
        "Services",
        "Provider backups of the VM",
        Scoped("services.view"),
        "Collection:Backup",
    ),
    status(
        op(
            "POST",
            "/api/v1/services/{id}/backups/{backup}/restore",
            "Services",
            "Restore a backup of this VM (destroys current data)",
            Scoped("services.manage"),
            "Queued",
        ),
        202,
    ),
    op(
        "GET",
        "/api/v1/services/{id}/reverse-dns",
        "Services",
        "Addresses of the VM with their PTR records",
        Scoped("services.view"),
        "Collection:ReverseDns",
    ),
    with_body(
        op(
            "PUT",
            "/api/v1/services/{id}/reverse-dns",
            "Services",
            "Set or reset the PTR of an address of the VM",
            Scoped("services.manage"),
            "ActionResult",
        ),
        "ReverseDnsInput",
        200,
    ),
    op(
        "GET",
        "/api/v1/services/{id}/isos",
        "Services",
        "ISO images the VM may mount (staff-approved)",
        Scoped("services.view"),
        "Collection:IsoImage",
    ),
    with_body(
        op(
            "POST",
            "/api/v1/services/{id}/iso",
            "Services",
            "Mount an approved ISO image",
            Scoped("services.manage"),
            "ActionResult",
        ),
        "IsoInput",
        200,
    ),
    op(
        "DELETE",
        "/api/v1/services/{id}/iso",
        "Services",
        "Eject the mounted ISO image",
        Scoped("services.manage"),
        "ActionResult",
    ),
    with_body(
        op(
            "POST",
            "/api/v1/services/{id}/rescue",
            "Services",
            "Enable or disable rescue mode",
            Scoped("services.manage"),
            "Rescue",
        ),
        "RescueInput",
        200,
    ),
    op(
        "GET",
        "/api/v1/services/{id}/firewall",
        "Services",
        "Firewall of the VM",
        Scoped("services.view"),
        "Firewall",
    ),
    with_body(
        op(
            "PUT",
            "/api/v1/services/{id}/firewall",
            "Services",
            "Replace the VM firewall (validated; safe defaults)",
            Scoped("services.manage"),
            "Firewall",
        ),
        "Firewall",
        200,
    ),
    with_query(
        op(
            "GET",
            "/api/v1/services/{id}/metrics",
            "Services",
            "Resource usage history",
            Scoped("services.view"),
            "MetricsHistory",
        ),
        &[("range", "hour, day (default), week or month")],
    ),
    op(
        "GET",
        "/api/v1/services/{id}/bandwidth",
        "Services",
        "Traffic of the current period against the quota",
        Scoped("services.view"),
        "Bandwidth",
    ),
    op(
        "GET",
        "/api/v1/services/{id}/operations",
        "Services",
        "VM operations with progress and failures",
        Scoped("services.view"),
        "Collection:VmOperation",
    ),
    with_query(
        op(
            "GET",
            "/api/v1/ssh-keys",
            "Services",
            "SSH keys of the client profile",
            Scoped("services.view"),
            "Collection:SshKey",
        ),
        &[("client_id", "Client (staff only; clients see their own)")],
    ),
    with_body(
        op(
            "POST",
            "/api/v1/ssh-keys",
            "Services",
            "Add an SSH public key",
            Scoped("services.manage"),
            "SshKey",
        ),
        "SshKeyInput",
        201,
    ),
    with_query(
        op(
            "DELETE",
            "/api/v1/ssh-keys/{id}",
            "Services",
            "Delete an SSH key",
            Scoped("services.manage"),
            "ActionResult",
        ),
        &[("client_id", "Client (staff only)")],
    ),
    listed(op(
        "GET",
        "/api/v1/domains",
        "Domains",
        "Domains",
        Scoped("domains.manage"),
        "List:Domain",
    )),
    with_query(
        op(
            "GET",
            "/api/v1/domains/availability",
            "Domains",
            "Availability and price of a domain",
            Public,
            "Availability",
        ),
        &[("domain", "Domain name, e.g. example.com (required)")],
    ),
    with_body(
        op(
            "POST",
            "/api/v1/domains/register",
            "Domains",
            "Order a registration or transfer",
            Scoped("billing.manage"),
            "OrderCreated",
        ),
        "DomainRegisterInput",
        201,
    ),
    op(
        "GET",
        "/api/v1/domains/{id}",
        "Domains",
        "A domain",
        Scoped("domains.manage"),
        "Domain",
    ),
    with_body(
        op(
            "POST",
            "/api/v1/domains/{id}/renew",
            "Domains",
            "Create a renewal invoice",
            Scoped("domains.manage"),
            "RenewResult",
        ),
        "RenewInput",
        201,
    ),
    with_body(
        op(
            "PUT",
            "/api/v1/domains/{id}/nameservers",
            "Domains",
            "Replace nameservers",
            Scoped("domains.manage"),
            "Object",
        ),
        "NameserversInput",
        200,
    ),
    with_body(
        op(
            "PUT",
            "/api/v1/domains/{id}/lock",
            "Domains",
            "Set the transfer lock",
            Scoped("domains.manage"),
            "Object",
        ),
        "LockInput",
        200,
    ),
    op(
        "GET",
        "/api/v1/departments",
        "Support",
        "Support departments",
        Scoped("support.view"),
        "Collection:Department",
    ),
    listed(op(
        "GET",
        "/api/v1/tickets",
        "Support",
        "Tickets",
        Scoped("support.view"),
        "List:Ticket",
    )),
    with_body(
        op(
            "POST",
            "/api/v1/tickets",
            "Support",
            "Open a ticket",
            Scoped("support.manage"),
            "Ticket",
        ),
        "TicketInput",
        201,
    ),
    op(
        "GET",
        "/api/v1/tickets/{id}",
        "Support",
        "A ticket with its messages",
        Scoped("support.view"),
        "Ticket",
    ),
    op(
        "GET",
        "/api/v1/tickets/{id}/messages",
        "Support",
        "Messages of a ticket",
        Scoped("support.view"),
        "Collection:TicketMessage",
    ),
    with_body(
        op(
            "POST",
            "/api/v1/tickets/{id}/messages",
            "Support",
            "Reply (staff may add internal notes)",
            Scoped("support.manage"),
            "TicketReply",
        ),
        "ReplyInput",
        201,
    ),
    // ---- network status (public, no authentication)
    op(
        "GET",
        "/api/v1/status",
        "Status",
        "Network status: public components, overall level and open incidents / maintenance",
        Public,
        "StatusSummary",
    ),
    op(
        "GET",
        "/api/v1/status/incidents",
        "Status",
        "Public incidents and maintenance of the last 90 days (newest 100)",
        Public,
        "Collection:StatusIncident",
    ),
    op(
        "GET",
        "/api/v1/status/incidents/{id}",
        "Status",
        "A public incident with its timeline",
        Public,
        "StatusIncident",
    ),
    // ---- affiliate account of the calling client (account owner only)
    op(
        "GET",
        "/api/v1/affiliate",
        "Affiliate",
        "The caller's affiliate account, figures and referral link (client account owner; 404 when not enrolled)",
        Any,
        "Affiliate",
    ),
    op(
        "GET",
        "/api/v1/affiliate/commissions",
        "Affiliate",
        "Commissions of the caller's affiliate account (newest 100)",
        Any,
        "Collection:AffiliateCommission",
    ),
    op(
        "GET",
        "/api/v1/affiliate/referrals",
        "Affiliate",
        "Referred clients (first name and initial only, newest 100)",
        Any,
        "Collection:AffiliateReferral",
    ),
    op(
        "GET",
        "/api/v1/affiliate/payouts",
        "Affiliate",
        "Payout requests of the caller's affiliate account (newest 100)",
        Any,
        "Collection:AffiliatePayout",
    ),
    // ---- staff to-do list
    with_query(
        op(
            "GET",
            "/api/v1/tasks",
            "Tasks",
            "Staff tasks (open first by due date, up to 300)",
            Staff("tasks.manage"),
            "Collection:Task",
        ),
        &[
            ("status", "open or done"),
            ("assigned_to", "staff user id"),
            ("client_id", "client id"),
        ],
    ),
    with_body(
        op(
            "POST",
            "/api/v1/tasks",
            "Tasks",
            "Create a task",
            Staff("tasks.manage"),
            "Task",
        ),
        "TaskInput",
        201,
    ),
    op(
        "GET",
        "/api/v1/tasks/{id}",
        "Tasks",
        "A task",
        Staff("tasks.manage"),
        "Task",
    ),
    with_body(
        op(
            "PATCH",
            "/api/v1/tasks/{id}",
            "Tasks",
            "Change a task (omitted fields stay; `status` completes or reopens it)",
            Staff("tasks.manage"),
            "Task",
        ),
        "TaskInput",
        200,
    ),
    op(
        "DELETE",
        "/api/v1/tasks/{id}",
        "Tasks",
        "Delete a task",
        Staff("tasks.manage"),
        "Deleted",
    ),
];

// ---------------------------------------------------------------------------
// schema helpers
// ---------------------------------------------------------------------------

fn t(ty: &str) -> Value {
    json!({ "type": ty })
}
fn uuid() -> Value {
    json!({ "type": "string", "format": "uuid" })
}
fn dt() -> Value {
    json!({ "type": "string", "format": "date-time" })
}
fn date() -> Value {
    json!({ "type": "string", "format": "date" })
}
fn money() -> Value {
    json!({ "type": "string", "pattern": "^-?[0-9]+\\.[0-9]{2}$", "description": "Decimal amount, two places", "examples": ["19.90"] })
}
fn null(v: Value) -> Value {
    json!({ "anyOf": [v, { "type": "null" }] })
}
fn enm(values: &[&str]) -> Value {
    json!({ "type": "string", "enum": values })
}
fn arr(items: Value) -> Value {
    json!({ "type": "array", "items": items })
}
fn r(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}
fn obj(props: &[(&str, Value)], required: &[&str]) -> Value {
    let mut p = Map::new();
    for (k, v) in props {
        p.insert((*k).to_owned(), v.clone());
    }
    json!({ "type": "object", "properties": p, "required": required })
}

fn schemas() -> Value {
    let currency = json!({ "type": "string", "pattern": "^[A-Z]{3}$", "examples": ["TRY"] });
    let s = t("string");
    let ns = null(t("string"));
    let b = t("boolean");
    let i = t("integer");
    let mut m = Map::new();
    let mut add = |name: &str, v: Value| {
        m.insert(name.to_owned(), v);
    };
    add(
        "Problem",
        obj(
            &[
                ("type", s.clone()),
                ("title", s.clone()),
                ("status", i.clone()),
                ("code", s.clone()),
                ("detail", s.clone()),
                ("errors", arr(r("FieldError"))),
            ],
            &["type", "title", "status", "code"],
        ),
    );
    add(
        "FieldError",
        obj(
            &[("field", s.clone()), ("message", s.clone())],
            &["field", "message"],
        ),
    );
    add(
        "Pagination",
        obj(
            &[
                ("total", i.clone()),
                ("limit", i.clone()),
                ("offset", i.clone()),
                ("has_more", b.clone()),
                ("next_offset", null(i.clone())),
            ],
            &["total", "limit", "offset", "has_more"],
        ),
    );
    add("Object", t("object"));
    add(
        "Deleted",
        obj(
            &[("id", uuid()), ("deleted", b.clone())],
            &["id", "deleted"],
        ),
    );
    add(
        "Ping",
        obj(&[("ok", b.clone()), ("tenant", s.clone())], &["ok"]),
    );
    add(
        "Me",
        obj(
            &[
                ("kind", enm(&["staff", "client"])),
                ("tenant", s.clone()),
                ("user_id", uuid()),
                ("client_id", uuid()),
                ("permissions", arr(s.clone())),
                ("via", enm(&["api_key", "session"])),
            ],
            &["kind", "user_id", "via"],
        ),
    );
    add(
        "Integration",
        obj(
            &[
                ("id", uuid()),
                (
                    "category",
                    enm(&[
                        "virt",
                        "panel",
                        "registrar",
                        "gateway",
                        "mail",
                        "ai",
                        "oauth",
                    ]),
                ),
                ("kind", s.clone()),
                ("name", s.clone()),
                ("active", b.clone()),
                ("default", b.clone()),
                ("has_secrets", b.clone()),
                ("last_test_at", null(dt())),
                ("last_test_ok", null(b.clone())),
                ("created_at", dt()),
            ],
            &["id", "category", "kind", "name"],
        ),
    );
    add(
        "WebhookEndpoint",
        obj(
            &[
                ("id", uuid()),
                ("url", s.clone()),
                ("description", ns.clone()),
                ("events", arr(s.clone())),
                ("active", b.clone()),
                ("last_delivery_at", null(dt())),
                ("last_status_code", null(i.clone())),
                ("failure_count", i.clone()),
                ("created_at", dt()),
            ],
            &["id", "url", "events"],
        ),
    );
    add(
        "WebhookEndpointCreated",
        json!({ "allOf": [r("WebhookEndpoint"), obj(&[("secret", json!({ "type": "string", "description": "whsec_… — shown once" }))], &["secret"])] }),
    );
    add(
        "WebhookEndpointInput",
        obj(
            &[
                ("url", json!({ "type": "string", "format": "uri" })),
                ("description", s.clone()),
                (
                    "events",
                    json!({ "type": "array", "items": { "type": "string" }, "description": "e.g. invoice.paid, service.active; empty or `*` = all" }),
                ),
            ],
            &["url"],
        ),
    );
    let price = obj(
        &[
            ("billing_cycle", s.clone()),
            ("price", money()),
            ("setup_fee", money()),
            ("currency", currency.clone()),
        ],
        &["billing_cycle", "price", "currency"],
    );
    add("Price", price);
    add(
        "CatalogProduct",
        obj(
            &[
                ("id", uuid()),
                ("slug", s.clone()),
                ("name", s.clone()),
                ("type", s.clone()),
                ("tagline", ns.clone()),
                ("description", ns.clone()),
                ("features", t("array")),
                ("featured", b.clone()),
                ("requires_domain", s.clone()),
                ("prices", arr(r("Price"))),
                ("config_options", arr(r("ConfigOption"))),
            ],
            &["id", "slug", "name", "prices"],
        ),
    );
    add(
        "ConfigOption",
        obj(
            &[
                ("id", uuid()),
                ("name", s.clone()),
                ("type", s.clone()),
                ("module_key", ns.clone()),
                ("unit", ns.clone()),
                ("min_quantity", i.clone()),
                ("max_quantity", i.clone()),
                (
                    "values",
                    arr(obj(
                        &[
                            ("id", uuid()),
                            ("label", s.clone()),
                            ("value", s.clone()),
                            ("prices", arr(r("Price"))),
                        ],
                        &["id", "label"],
                    )),
                ),
            ],
            &["id", "name", "values"],
        ),
    );
    add(
        "Catalog",
        obj(
            &[
                ("currency", currency.clone()),
                (
                    "groups",
                    arr(obj(
                        &[
                            ("id", uuid()),
                            ("slug", s.clone()),
                            ("name", s.clone()),
                            ("tagline", ns.clone()),
                            ("products", arr(r("CatalogProduct"))),
                        ],
                        &["id", "slug", "name", "products"],
                    )),
                ),
            ],
            &["currency", "groups"],
        ),
    );
    add(
        "Tld",
        obj(
            &[
                ("tld", s.clone()),
                ("label", ns.clone()),
                ("featured", b.clone()),
                ("register", null(money())),
                ("transfer", null(money())),
                ("renew", null(money())),
                ("currency", currency.clone()),
            ],
            &["tld", "currency"],
        ),
    );
    add(
        "Product",
        obj(
            &[
                ("id", uuid()),
                ("slug", s.clone()),
                ("name", s.clone()),
                ("type", s.clone()),
                ("group_id", null(uuid())),
                ("tagline", ns.clone()),
                ("description", ns.clone()),
                ("features", t("array")),
                ("pay_type", enm(&["free", "one_time", "recurring"])),
                ("hidden", b.clone()),
                ("retired", b.clone()),
                ("featured", b.clone()),
                ("stock_control", b.clone()),
                ("stock_quantity", i.clone()),
                (
                    "prices",
                    arr(json!({ "allOf": [r("Price"), obj(&[("enabled", b.clone())], &[])] })),
                ),
                ("created_at", dt()),
                ("updated_at", dt()),
            ],
            &["id", "slug", "name", "type"],
        ),
    );
    let client_props = [
        ("kind", enm(&["individual", "corporate"])),
        ("first_name", s.clone()),
        ("last_name", s.clone()),
        ("company_name", ns.clone()),
        ("email", json!({ "type": "string", "format": "email" })),
        ("phone", ns.clone()),
        ("address_line1", ns.clone()),
        ("address_line2", ns.clone()),
        ("city", ns.clone()),
        ("state", ns.clone()),
        ("postal_code", ns.clone()),
        ("country_code", s.clone()),
        ("tax_office", ns.clone()),
        ("tax_number", ns.clone()),
        ("currency", currency.clone()),
        ("locale", enm(&["tr", "en"])),
        ("tax_exempt", b.clone()),
        ("marketing_opt_in", b.clone()),
    ];
    add(
        "ClientInput",
        obj(&client_props, &["first_name", "last_name", "email"]),
    );
    let mut client = client_props.to_vec();
    client.extend([
        ("id", uuid()),
        ("status", enm(&["active", "inactive", "closed"])),
        ("created_at", dt()),
        ("updated_at", dt()),
    ]);
    add("Client", obj(&client, &["id", "email", "status"]));
    add(
        "OrderItemInput",
        obj(
            &[
                ("product_id", uuid()),
                (
                    "billing_cycle",
                    enm(&[
                        "monthly",
                        "quarterly",
                        "semiannual",
                        "annual",
                        "biennial",
                        "triennial",
                        "one_time",
                    ]),
                ),
                ("quantity", i.clone()),
                ("hostname", s.clone()),
                (
                    "config_options",
                    json!({
                        "type": "object",
                        "description": "Product lines: { \"<option id>\": { \"value_id\": \"<value id>\", \"quantity\": 1 } } (ids from GET /api/v1/catalog)",
                        "additionalProperties": obj(&[("value_id", uuid()), ("quantity", i.clone())], &["value_id"]),
                    }),
                ),
                (
                    "domain",
                    json!({ "type": "string", "description": "Domain lines: name to register / transfer. Product lines: the hosting domain." }),
                ),
                ("action", enm(&["register", "transfer"])),
                ("years", i.clone()),
            ],
            &[],
        ),
    );
    add(
        "OrderInput",
        obj(
            &[
                (
                    "client_id",
                    json!({ "type": "string", "format": "uuid", "description": "Required for staff keys; ignored for client keys" }),
                ),
                ("currency", currency.clone()),
                ("items", arr(r("OrderItemInput"))),
                ("coupon_code", s.clone()),
                ("payment_integration_id", uuid()),
                ("notes", s.clone()),
            ],
            &["items"],
        ),
    );
    add(
        "QuoteLine",
        obj(
            &[
                ("index", i.clone()),
                ("parent", null(i.clone())),
                ("type", s.clone()),
                ("description", s.clone()),
                ("product_id", null(uuid())),
                ("domain_name", ns.clone()),
                ("billing_cycle", ns.clone()),
                ("years", null(i.clone())),
                ("quantity", i.clone()),
                ("unit_price", money()),
                ("setup_fee", money()),
                ("discount", money()),
                ("amount", money()),
                ("recurring_amount", money()),
            ],
            &["index", "type", "amount"],
        ),
    );
    add(
        "Quote",
        obj(
            &[
                ("currency", currency.clone()),
                ("lines", arr(r("QuoteLine"))),
                ("coupon_error", ns.clone()),
                ("prices_include_tax", b.clone()),
                ("subtotal", money()),
                ("discount_total", money()),
                ("setup_total", money()),
                ("tax_total", money()),
                ("total", money()),
            ],
            &["currency", "lines", "total"],
        ),
    );
    add(
        "OrderCreated",
        obj(
            &[
                ("order_id", uuid()),
                ("order_number", s.clone()),
                ("invoice_id", uuid()),
                ("invoice_number", ns.clone()),
                ("service_ids", arr(uuid())),
                ("domain_ids", arr(uuid())),
                ("currency", currency.clone()),
                ("total", money()),
                ("credit_applied", money()),
                ("balance", money()),
                ("paid", b.clone()),
                ("quote", r("Quote")),
                ("next", null(obj(&[("pay", s.clone())], &[]))),
            ],
            &["order_id", "invoice_id", "total", "paid"],
        ),
    );
    add(
        "Order",
        obj(
            &[
                ("id", uuid()),
                ("number", s.clone()),
                ("client_id", uuid()),
                (
                    "status",
                    enm(&["pending", "accepted", "cancelled", "fraud"]),
                ),
                ("currency", currency.clone()),
                ("subtotal", money()),
                ("discount_total", money()),
                ("tax_total", money()),
                ("total", money()),
                ("coupon_code", ns.clone()),
                ("invoice_id", null(uuid())),
                ("notes", ns.clone()),
                ("accepted_at", null(dt())),
                ("cancelled_at", null(dt())),
                ("created_at", dt()),
                (
                    "items",
                    arr(obj(
                        &[
                            ("id", uuid()),
                            ("type", s.clone()),
                            ("description", s.clone()),
                            ("product_id", null(uuid())),
                            ("service_id", null(uuid())),
                            ("domain_id", null(uuid())),
                            ("domain_name", ns.clone()),
                            ("billing_cycle", ns.clone()),
                            ("years", null(i.clone())),
                            ("quantity", i.clone()),
                            ("unit_price", money()),
                            ("setup_fee", money()),
                            ("discount", money()),
                            ("amount", money()),
                        ],
                        &["id", "type", "amount"],
                    )),
                ),
            ],
            &["id", "number", "status", "total"],
        ),
    );
    add(
        "Transaction",
        obj(
            &[
                ("id", uuid()),
                ("client_id", uuid()),
                ("invoice_id", null(uuid())),
                ("gateway", s.clone()),
                ("gateway_txn_id", ns.clone()),
                ("kind", enm(&["payment", "refund", "chargeback"])),
                ("status", enm(&["pending", "succeeded", "failed"])),
                ("amount", money()),
                ("fee", money()),
                ("currency", currency.clone()),
                ("description", ns.clone()),
                ("occurred_at", dt()),
            ],
            &["id", "kind", "status", "amount", "currency"],
        ),
    );
    add(
        "Invoice",
        obj(
            &[
                ("id", uuid()),
                ("number", ns.clone()),
                ("client_id", uuid()),
                (
                    "status",
                    enm(&[
                        "draft",
                        "unpaid",
                        "payment_pending",
                        "paid",
                        "partially_refunded",
                        "refunded",
                        "cancelled",
                        "collections",
                    ]),
                ),
                ("currency", currency.clone()),
                ("issue_date", date()),
                ("due_date", date()),
                ("paid_at", null(dt())),
                ("subtotal", money()),
                ("discount_total", money()),
                ("tax_total", money()),
                ("total", money()),
                ("amount_paid", money()),
                ("credit_applied", money()),
                ("balance", money()),
                ("notes", ns.clone()),
                ("created_at", dt()),
                (
                    "items",
                    arr(obj(
                        &[
                            ("id", uuid()),
                            ("type", s.clone()),
                            ("description", s.clone()),
                            ("quantity", s.clone()),
                            ("unit_price", money()),
                            ("amount", money()),
                            ("taxable", b.clone()),
                            ("service_id", null(uuid())),
                            ("domain_id", null(uuid())),
                            ("period_start", null(date())),
                            ("period_end", null(date())),
                        ],
                        &["id", "description", "amount"],
                    )),
                ),
                ("transactions", arr(r("Transaction"))),
            ],
            &["id", "status", "currency", "total", "balance"],
        ),
    );
    add(
        "PayInput",
        obj(
            &[
                (
                    "integration_id",
                    json!({ "type": "string", "format": "uuid", "description": "Gateway; default gateway when omitted" }),
                ),
                ("return_url", json!({ "type": "string", "format": "uri" })),
                ("cancel_url", json!({ "type": "string", "format": "uri" })),
            ],
            &[],
        ),
    );
    add(
        "PaymentSession",
        obj(
            &[
                ("invoice_id", uuid()),
                ("reference", s.clone()),
                ("integration_id", uuid()),
                ("amount", money()),
                ("currency", currency.clone()),
                (
                    "session",
                    obj(
                        &[
                            (
                                "type",
                                enm(&["redirect", "form", "iframe", "embed", "offline"]),
                            ),
                            ("url", s.clone()),
                            ("action", s.clone()),
                            ("method", enm(&["GET", "POST"])),
                            (
                                "fields",
                                arr(obj(
                                    &[("name", s.clone()), ("value", s.clone())],
                                    &["name", "value"],
                                )),
                            ),
                            ("html", s.clone()),
                            ("instructions", s.clone()),
                            ("expires_at", null(dt())),
                        ],
                        &["type"],
                    ),
                ),
            ],
            &["reference", "amount", "currency", "session"],
        ),
    );
    add(
        "ManualPaymentInput",
        obj(
            &[
                ("amount", money()),
                ("integration_id", uuid()),
                ("method", s.clone()),
                ("reference", s.clone()),
                ("occurred_at", dt()),
                ("note", s.clone()),
            ],
            &["amount"],
        ),
    );
    add(
        "ManualPaymentResult",
        obj(
            &[
                ("transaction", r("Transaction")),
                ("duplicate", b.clone()),
                ("invoice_paid", b.clone()),
                ("credited", money()),
                ("invoice", null(r("Invoice"))),
            ],
            &["transaction", "invoice_paid"],
        ),
    );
    add(
        "Service",
        obj(
            &[
                ("id", uuid()),
                ("client_id", uuid()),
                ("product_id", uuid()),
                ("order_id", null(uuid())),
                (
                    "status",
                    enm(&[
                        "pending",
                        "provisioning",
                        "active",
                        "suspended",
                        "terminated",
                        "cancelled",
                        "failed",
                    ]),
                ),
                ("billing_cycle", s.clone()),
                ("first_payment_amount", money()),
                ("recurring_amount", money()),
                ("currency", currency.clone()),
                ("quantity", i.clone()),
                ("domain", ns.clone()),
                ("username", ns.clone()),
                ("server_hostname", ns.clone()),
                ("dedicated_ip", ns.clone()),
                ("assigned_ips", arr(s.clone())),
                ("registration_date", date()),
                ("next_due_date", null(date())),
                ("suspended_at", null(dt())),
                ("suspended_reason", ns.clone()),
                ("terminated_at", null(dt())),
                ("created_at", dt()),
                ("product_name", s.clone()),
                ("product_type", s.clone()),
                ("client_name", s.clone()),
            ],
            &["id", "client_id", "status"],
        ),
    );
    add(
        "Queued",
        obj(
            &[
                ("service_id", uuid()),
                ("action", s.clone()),
                ("queued", b.clone()),
            ],
            &["service_id", "action"],
        ),
    );
    add(
        "ActionResult",
        obj(&[("service_id", uuid())], &["service_id"]),
    );
    add("ReasonInput", obj(&[("reason", s.clone())], &[]));
    add(
        "PowerInput",
        obj(
            &[(
                "action",
                enm(&["start", "stop", "shutdown", "reboot", "reset"]),
            )],
            &["action"],
        ),
    );
    add(
        "ReinstallInput",
        obj(
            &[("template", s.clone()), ("ssh_keys", arr(s.clone()))],
            &["template"],
        ),
    );
    add(
        "SnapshotInput",
        obj(
            &[(
                "name",
                json!({ "type": "string", "pattern": "^[A-Za-z0-9_-]{1,40}$" }),
            )],
            &["name"],
        ),
    );
    add(
        "Snapshot",
        obj(
            &[
                ("id", s.clone()),
                ("name", s.clone()),
                ("description", ns.clone()),
                ("created_at", null(dt())),
                ("size_mb", null(i.clone())),
                ("includes_memory", b.clone()),
            ],
            &["id", "name"],
        ),
    );
    add(
        "PasswordResetInput",
        obj(
            &[("username", ns.clone()), ("confirm", b.clone())],
            &["confirm"],
        ),
    );
    add(
        "PasswordReset",
        obj(
            &[
                ("service_id", uuid()),
                ("username", s.clone()),
                ("password", s.clone()),
            ],
            &["service_id", "username", "password"],
        ),
    );
    add(
        "Backup",
        obj(
            &[
                ("id", s.clone()),
                ("name", s.clone()),
                ("created_at", null(dt())),
                ("size_mb", null(i.clone())),
            ],
            &["id", "name"],
        ),
    );
    add(
        "ReverseDns",
        obj(
            &[
                ("ip", s.clone()),
                ("ptr", ns.clone()),
                ("editable", b.clone()),
            ],
            &["ip"],
        ),
    );
    add(
        "ReverseDnsInput",
        obj(&[("ip", s.clone()), ("ptr", ns.clone())], &["ip"]),
    );
    add(
        "IsoImage",
        obj(&[("id", s.clone()), ("name", s.clone())], &["id", "name"]),
    );
    add(
        "IsoInput",
        obj(&[("iso", s.clone()), ("boot", b.clone())], &["iso"]),
    );
    add(
        "RescueInput",
        obj(
            &[("enabled", b.clone()), ("ssh_key_ids", arr(uuid()))],
            &["enabled"],
        ),
    );
    add(
        "Rescue",
        obj(
            &[
                ("service_id", uuid()),
                ("enabled", b.clone()),
                ("password", ns.clone()),
            ],
            &["service_id", "enabled"],
        ),
    );
    add(
        "FirewallRule",
        obj(
            &[
                ("direction", enm(&["in", "out"])),
                ("protocol", enm(&["tcp", "udp", "icmp"])),
                ("port", ns.clone()),
                ("cidrs", arr(s.clone())),
                ("comment", ns.clone()),
            ],
            &["direction", "protocol"],
        ),
    );
    add(
        "Firewall",
        obj(
            &[("enabled", b.clone()), ("rules", arr(r("FirewallRule")))],
            &["enabled", "rules"],
        ),
    );
    add(
        "MetricSample",
        obj(
            &[
                ("at", null(dt())),
                ("cpu_percent", null(t("number"))),
                ("memory_used_mb", null(t("number"))),
                ("memory_total_mb", null(t("number"))),
                ("disk_read_bps", null(t("number"))),
                ("disk_write_bps", null(t("number"))),
                ("net_in_bps", null(t("number"))),
                ("net_out_bps", null(t("number"))),
            ],
            &[],
        ),
    );
    add(
        "MetricsHistory",
        obj(
            &[
                ("range", enm(&["hour", "day", "week", "month"])),
                ("step_secs", i.clone()),
                ("samples", arr(r("MetricSample"))),
            ],
            &["range", "samples"],
        ),
    );
    add(
        "Bandwidth",
        obj(
            &[
                ("service_id", uuid()),
                ("counted_gb", t("number")),
                ("incoming_gb", null(t("number"))),
                ("outgoing_gb", null(t("number"))),
                ("estimated", b.clone()),
                ("period_start", null(dt())),
                ("quota_gb", null(t("number"))),
                ("used_percent", null(t("number"))),
                ("overage_gb", t("number")),
            ],
            &["service_id", "counted_gb", "overage_gb"],
        ),
    );
    add(
        "VmOperation",
        obj(
            &[
                ("id", uuid()),
                ("kind", s.clone()),
                ("status", enm(&["queued", "running", "succeeded", "failed"])),
                ("progress", null(i.clone())),
                ("detail", s.clone()),
                ("error", ns.clone()),
                ("created_at", dt()),
                ("finished_at", null(dt())),
            ],
            &["id", "kind", "status"],
        ),
    );
    add(
        "SshKey",
        obj(
            &[
                ("id", uuid()),
                ("client_id", uuid()),
                ("name", s.clone()),
                ("public_key", s.clone()),
                ("fingerprint", s.clone()),
                ("created_at", dt()),
            ],
            &["id", "name", "public_key", "fingerprint"],
        ),
    );
    add(
        "SshKeyInput",
        obj(
            &[
                ("name", s.clone()),
                ("public_key", s.clone()),
                ("client_id", null(uuid())),
            ],
            &["name", "public_key"],
        ),
    );
    add(
        "Console",
        obj(
            &[
                ("service_id", uuid()),
                (
                    "kind",
                    enm(&["novnc", "spice", "xterm", "webmks", "url", "embedded"]),
                ),
                ("url", s.clone()),
                ("ticket", ns.clone()),
                ("password", ns.clone()),
                ("port", null(i.clone())),
                ("expires_at", null(dt())),
            ],
            &["kind", "url"],
        ),
    );
    add(
        "Domain",
        obj(
            &[
                ("id", uuid()),
                ("client_id", uuid()),
                ("name", s.clone()),
                ("tld", s.clone()),
                ("status", s.clone()),
                ("registration_type", enm(&["register", "transfer"])),
                ("registration_period", i.clone()),
                ("registered_at", null(dt())),
                ("expires_at", null(dt())),
                ("next_due_date", null(date())),
                ("recurring_amount", money()),
                ("currency", currency.clone()),
                ("auto_renew", b.clone()),
                ("privacy", b.clone()),
                ("locked", b.clone()),
                ("nameservers", arr(s.clone())),
                ("last_synced_at", null(dt())),
                ("created_at", dt()),
            ],
            &["id", "name", "status"],
        ),
    );
    add(
        "Availability",
        obj(
            &[
                ("domain", s.clone()),
                ("tld", s.clone()),
                (
                    "status",
                    enm(&["available", "taken", "unknown", "unsupported"]),
                ),
                ("available", b.clone()),
                ("premium", b.clone()),
                ("source", enm(&["registrar", "rdap"])),
                ("currency", currency.clone()),
                ("register", null(money())),
                ("transfer", null(money())),
                ("renew", null(money())),
            ],
            &["domain", "status", "available"],
        ),
    );
    add(
        "DomainRegisterInput",
        obj(
            &[
                ("domain", s.clone()),
                ("years", i.clone()),
                ("action", enm(&["register", "transfer"])),
                ("client_id", uuid()),
                ("payment_integration_id", uuid()),
                ("currency", currency.clone()),
            ],
            &["domain"],
        ),
    );
    add("RenewInput", obj(&[("years", i.clone())], &[]));
    add(
        "RenewResult",
        obj(
            &[
                ("domain_id", uuid()),
                ("years", i.clone()),
                ("invoice_id", uuid()),
            ],
            &["invoice_id"],
        ),
    );
    add(
        "NameserversInput",
        obj(&[("nameservers", arr(s.clone()))], &["nameservers"]),
    );
    add("LockInput", obj(&[("locked", b.clone())], &["locked"]));
    add(
        "Department",
        obj(
            &[
                ("id", uuid()),
                ("slug", s.clone()),
                ("name", s.clone()),
                ("description", ns.clone()),
                ("clients_only", b.clone()),
            ],
            &["id", "name"],
        ),
    );
    add(
        "TicketMessage",
        obj(
            &[
                ("id", uuid()),
                ("author_kind", enm(&["client", "staff", "system", "guest"])),
                ("author_name", s.clone()),
                ("body", s.clone()),
                ("internal", b.clone()),
                ("source", enm(&["web", "email", "api"])),
                ("created_at", dt()),
            ],
            &["id", "author_kind", "body"],
        ),
    );
    add(
        "Ticket",
        obj(
            &[
                ("id", uuid()),
                ("number", s.clone()),
                ("subject", s.clone()),
                (
                    "status",
                    enm(&[
                        "open",
                        "answered",
                        "customer_reply",
                        "in_progress",
                        "on_hold",
                        "closed",
                    ]),
                ),
                ("priority", enm(&["low", "medium", "high", "urgent"])),
                ("department_id", uuid()),
                ("client_id", null(uuid())),
                ("service_id", null(uuid())),
                ("domain_id", null(uuid())),
                ("last_reply_at", dt()),
                ("last_reply_by", s.clone()),
                ("closed_at", null(dt())),
                ("created_at", dt()),
                ("messages", arr(r("TicketMessage"))),
            ],
            &["id", "number", "subject", "status"],
        ),
    );
    add(
        "TicketInput",
        obj(
            &[
                ("subject", s.clone()),
                ("message", s.clone()),
                ("department_id", uuid()),
                ("priority", enm(&["low", "medium", "high", "urgent"])),
                ("client_id", uuid()),
                ("service_id", uuid()),
                ("domain_id", uuid()),
            ],
            &["subject", "message"],
        ),
    );
    add(
        "ReplyInput",
        obj(
            &[("message", s.clone()), ("internal", b.clone())],
            &["message"],
        ),
    );
    add(
        "SalesQuoteItem",
        obj(
            &[
                ("description", s.clone()),
                ("product_id", null(uuid())),
                ("billing_cycle", null(t("string"))),
                ("quantity", s.clone()),
                ("unit_price", money()),
                ("discount_percent", s.clone()),
                ("amount", money()),
                ("taxable", b.clone()),
            ],
            &["description", "quantity", "unit_price", "amount"],
        ),
    );
    add(
        "SalesQuote",
        obj(
            &[
                ("id", uuid()),
                ("number", s.clone()),
                ("client_id", null(uuid())),
                ("subject", s.clone()),
                (
                    "status",
                    enm(&[
                        "draft",
                        "sent",
                        "accepted",
                        "declined",
                        "expired",
                        "cancelled",
                    ]),
                ),
                ("currency", currency.clone()),
                ("valid_until", date()),
                ("subtotal", money()),
                ("discount_total", money()),
                ("tax_total", money()),
                ("total", money()),
                ("notes", ns.clone()),
                ("sent_at", null(dt())),
                ("accepted_at", null(dt())),
                ("declined_at", null(dt())),
                ("order_id", null(uuid())),
                ("invoice_id", null(uuid())),
                ("created_at", dt()),
                ("items", arr(r("SalesQuoteItem"))),
            ],
            &["id", "number", "subject", "status", "currency", "total"],
        ),
    );
    add(
        "QuoteAcceptance",
        obj(
            &[
                ("quote_id", uuid()),
                ("order_id", uuid()),
                ("invoice_id", uuid()),
                ("paid", b.clone()),
                ("next", json!({ "type": ["object", "null"] })),
            ],
            &["quote_id", "order_id", "invoice_id", "paid"],
        ),
    );
    add("QuoteDecline", obj(&[("reason", ns.clone())], &[]));
    add(
        "StatusComponent",
        obj(
            &[
                ("id", uuid()),
                ("name", s.clone()),
                ("group", s.clone()),
                ("description", ns.clone()),
                (
                    "status",
                    enm(&[
                        "operational",
                        "degraded",
                        "partial_outage",
                        "major_outage",
                        "maintenance",
                    ]),
                ),
                ("updated_at", dt()),
            ],
            &["id", "name", "group", "status"],
        ),
    );
    add(
        "StatusIncident",
        obj(
            &[
                ("id", uuid()),
                ("kind", enm(&["incident", "maintenance"])),
                ("title", s.clone()),
                ("impact", s.clone()),
                ("status", s.clone()),
                ("scheduled_start", null(dt())),
                ("scheduled_end", null(dt())),
                ("started_at", dt()),
                ("resolved_at", null(dt())),
                ("component_ids", arr(uuid())),
                ("updated_at", dt()),
                (
                    "updates",
                    arr(obj(
                        &[
                            ("status", s.clone()),
                            ("body", s.clone()),
                            ("created_at", dt()),
                        ],
                        &["status", "body", "created_at"],
                    )),
                ),
            ],
            &["id", "kind", "title", "impact", "status", "started_at"],
        ),
    );
    add(
        "StatusSummary",
        obj(
            &[
                ("level", enm(&["ok", "maintenance", "minor", "major"])),
                ("components", arr(r("StatusComponent"))),
                ("incidents", arr(r("StatusIncident"))),
            ],
            &["level", "components", "incidents"],
        ),
    );
    add(
        "Affiliate",
        obj(
            &[
                ("id", uuid()),
                ("code", s.clone()),
                ("status", s.clone()),
                ("currency", currency.clone()),
                ("link", s.clone()),
                ("referrals", i.clone()),
                ("clicks", i.clone()),
                ("pending", money()),
                ("balance", money()),
                ("paid_out", money()),
                ("created_at", dt()),
            ],
            &["id", "code", "status", "currency", "link", "balance"],
        ),
    );
    add(
        "AffiliateCommission",
        obj(
            &[
                ("id", uuid()),
                ("kind", s.clone()),
                ("base_amount", money()),
                ("amount", money()),
                ("currency", currency.clone()),
                ("status", s.clone()),
                ("clears_at", dt()),
                ("reversed_at", null(dt())),
                ("created_at", dt()),
            ],
            &["id", "amount", "currency", "status"],
        ),
    );
    add(
        "AffiliateReferral",
        obj(
            &[
                ("id", uuid()),
                ("name", s.clone()),
                ("earned", money()),
                ("currency", currency.clone()),
                ("created_at", dt()),
            ],
            &["id", "name", "earned"],
        ),
    );
    add(
        "AffiliatePayout",
        obj(
            &[
                ("id", uuid()),
                ("amount", money()),
                ("currency", currency.clone()),
                ("method", s.clone()),
                ("status", s.clone()),
                ("processed_at", null(dt())),
                ("created_at", dt()),
            ],
            &["id", "amount", "currency", "status"],
        ),
    );
    add(
        "Task",
        obj(
            &[
                ("id", uuid()),
                ("title", s.clone()),
                ("body", ns.clone()),
                ("status", enm(&["open", "done"])),
                ("priority", enm(&["low", "normal", "high"])),
                ("due_date", null(date())),
                ("assigned_to", null(uuid())),
                ("client_id", null(uuid())),
                ("created_by", null(uuid())),
                ("completed_at", null(dt())),
                ("created_at", dt()),
                ("updated_at", dt()),
            ],
            &["id", "title", "status", "priority"],
        ),
    );
    add(
        "TaskInput",
        obj(
            &[
                ("title", s.clone()),
                ("body", ns.clone()),
                ("priority", enm(&["low", "normal", "high"])),
                ("due_date", null(date())),
                ("assigned_to", null(uuid())),
                ("client_id", null(uuid())),
                ("status", enm(&["open", "done"])),
            ],
            &[],
        ),
    );
    add(
        "TicketReply",
        obj(
            &[("ticket", r("Ticket")), ("message", r("TicketMessage"))],
            &["ticket", "message"],
        ),
    );
    Value::Object(m)
}

fn list_params() -> Value {
    let q = |name: &str, desc: &str, schema: Value| json!({ "name": name, "in": "query", "required": false, "description": desc, "schema": schema });
    json!({
        "limit": q("limit", "Page size (1–200, default 25)", json!({ "type": "integer", "minimum": 1, "maximum": 200 })),
        "offset": q("offset", "Rows to skip", json!({ "type": "integer", "minimum": 0 })),
        "sort": q("sort", "Sort key, `-` prefix for descending (default: newest first); allowed keys per resource", t("string")),
        "status": q("status", "Filter by status", t("string")),
        "q": q("q", "Case-insensitive search", t("string")),
        "client_id": q("client_id", "Staff only: filter by client", uuid()),
        "created_from": q("created_from", "created_at ≥ (RFC 3339)", dt()),
        "created_to": q("created_to", "created_at < (RFC 3339)", dt()),
        "IdempotencyKey": {
            "name": "Idempotency-Key", "in": "header", "required": false,
            "description": "Makes the POST safe to retry: the first response is replayed for 24 h.",
            "schema": { "type": "string", "maxLength": 255 }
        }
    })
}

fn problem(desc: &str) -> Value {
    json!({
        "description": desc,
        "content": { "application/problem+json": { "schema": r("Problem") } }
    })
}

fn response_schema(name: &str) -> Value {
    if let Some(inner) = name.strip_prefix("List:") {
        obj(
            &[("data", arr(r(inner))), ("pagination", r("Pagination"))],
            &["data", "pagination"],
        )
    } else if let Some(inner) = name.strip_prefix("Collection:") {
        obj(
            &[("total", t("integer")), ("data", arr(r(inner)))],
            &["data"],
        )
    } else {
        r(name)
    }
}

fn operation_id(o: &Operation) -> String {
    let mut id = o.method.to_ascii_lowercase();
    for seg in o.path.trim_start_matches("/api/v1/").split('/') {
        let seg = seg.trim_matches(|c| c == '{' || c == '}');
        id.push('_');
        id.push_str(&seg.replace('-', "_"));
    }
    id
}

fn access_text(a: Access) -> String {
    match a {
        Public => "Public — no authentication.".to_owned(),
        Any => "Any API key or session.".to_owned(),
        Scoped(p) => format!(
            "Staff: permission `{p}`. Client keys: only their own records (others answer 404)."
        ),
        Staff(p) => format!("Staff only: permission `{p}` (client keys get 403)."),
    }
}

/// The OpenAPI 3.1 document.
pub fn document() -> Value {
    let mut paths = Map::new();
    for o in OPERATIONS {
        let mut params: Vec<Value> = Vec::new();
        for seg in o.path.split('/') {
            if let Some(name) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                params.push(json!({
                    "name": name, "in": "path", "required": true,
                    "schema": if name == "snapshot" { t("string") } else { uuid() },
                }));
            }
        }
        if o.list {
            for p in [
                "limit",
                "offset",
                "sort",
                "status",
                "q",
                "client_id",
                "created_from",
                "created_to",
            ] {
                params.push(json!({ "$ref": format!("#/components/parameters/{p}") }));
            }
        }
        for (name, desc) in o.query {
            params.push(json!({ "name": name, "in": "query", "required": *name == "domain", "description": desc, "schema": t("string") }));
        }
        if o.method == "POST" {
            params.push(json!({ "$ref": "#/components/parameters/IdempotencyKey" }));
        }
        let mut responses = Map::new();
        let success = match o.response.strip_prefix("Binary:") {
            Some(media) => json!({
                "description": "Success (file download)",
                "headers": { "Content-Disposition": { "schema": t("string") } },
                "content": { media: { "schema": { "type": "string", "contentMediaType": media } } }
            }),
            None => json!({
                "description": "Success",
                "content": { "application/json": { "schema": response_schema(o.response) } }
            }),
        };
        responses.insert(o.status.to_string(), success);
        if o.access != Public {
            responses.insert(
                "401".into(),
                json!({ "$ref": "#/components/responses/Unauthorized" }),
            );
            responses.insert(
                "403".into(),
                json!({ "$ref": "#/components/responses/Forbidden" }),
            );
        }
        if o.path.contains('{') {
            responses.insert(
                "404".into(),
                json!({ "$ref": "#/components/responses/NotFound" }),
            );
        }
        if o.request.is_some() || o.list || !o.query.is_empty() {
            responses.insert(
                "422".into(),
                json!({ "$ref": "#/components/responses/ValidationFailed" }),
            );
        }
        responses.insert(
            "429".into(),
            json!({ "$ref": "#/components/responses/RateLimited" }),
        );
        let mut operation = json!({
            "operationId": operation_id(o),
            "tags": [o.tag],
            "summary": o.summary,
            "description": access_text(o.access),
            "parameters": params,
            "responses": responses,
        });
        if o.access == Public {
            operation["security"] = json!([]);
        }
        if let Some(req) = o.request {
            operation["requestBody"] = json!({
                "required": true,
                "content": { "application/json": { "schema": r(req) } }
            });
        }
        let entry = paths
            .entry(o.path.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        entry[o.method.to_ascii_lowercase()] = operation;
    }
    let mut tags: Vec<&str> = OPERATIONS.iter().map(|o| o.tag).collect();
    tags.dedup();
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "HostingOS API",
            "version": "1.0.0",
            "description": "Tenant REST API. Call it on the tenant's own host (`https://<slug>.<base domain>` or a verified custom domain). Errors are RFC 9457 problem documents; money is a decimal string next to a currency code.",
        },
        "servers": [{ "url": "/", "description": "The tenant host the request is sent to" }],
        "security": [{ "bearerAuth": [] }, { "sessionCookie": [] }],
        "tags": tags.iter().map(|t| json!({ "name": t })).collect::<Vec<_>>(),
        "paths": paths,
        "components": {
            "securitySchemes": {
                "bearerAuth": { "type": "http", "scheme": "bearer", "bearerFormat": "hos_live_…", "description": "API key from Admin → API keys (staff) or Client area → API keys." },
                "sessionCookie": { "type": "apiKey", "in": "cookie", "name": "hos_session", "description": "Browser session; unsafe methods then need the X-CSRF-Token header." }
            },
            "parameters": list_params(),
            "responses": {
                "Unauthorized": problem("Missing or invalid credentials"),
                "Forbidden": problem("Authenticated but not allowed (missing permission, staff-only endpoint)"),
                "NotFound": problem("Not found — also returned for records of other clients / tenants"),
                "ValidationFailed": problem("Invalid fields (`errors` lists them)"),
                "RateLimited": problem("Rate limit exceeded; see Retry-After and RateLimit-* headers"),
            },
            "schemas": schemas(),
        }
    })
}

/// `GET /api/v1/openapi.json`.
pub async fn document_handler() -> Response {
    let mut res = axum::Json(document()).into_response();
    res.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("public, max-age=300"),
    );
    res
}

/// `GET /api/docs` — a readable reference generated from [`OPERATIONS`].
pub async fn docs_page(State(state): State<AppState>) -> Html<String> {
    let mut by_tag: Vec<(&str, Vec<&Operation>)> = Vec::new();
    for o in OPERATIONS {
        match by_tag.iter_mut().find(|(t, _)| *t == o.tag) {
            Some((_, v)) => v.push(o),
            None => by_tag.push((o.tag, vec![o])),
        }
    }
    let mut nav = String::new();
    let mut body = String::new();
    for (tag, ops) in &by_tag {
        let anchor = tag.to_ascii_lowercase();
        nav.push_str(&format!(
            r##"<li><a href="#{anchor}">{}</a></li>"##,
            escape_html(tag)
        ));
        body.push_str(&format!(
            r#"<section class="card mt-6" id="{anchor}"><header class="card__head"><div><h2>{}</h2></div></header><div class="card__body"><div class="table-wrap"><table class="table"><thead><tr><th scope="col">Endpoint</th><th scope="col">Access</th><th scope="col">Body → response</th></tr></thead><tbody>"#,
            escape_html(tag)
        ));
        for o in ops {
            let tone = match o.method {
                "GET" => "info",
                "DELETE" => "danger",
                _ => "success",
            };
            let mut extra = String::new();
            if o.list {
                extra.push_str("<br><span class=\"muted small\">limit, offset, sort, status, q, client_id, created_from, created_to</span>");
            }
            for (name, desc) in o.query {
                extra.push_str(&format!(
                    "<br><span class=\"muted small\"><code>{}</code> — {}</span>",
                    escape_html(name),
                    escape_html(desc)
                ));
            }
            body.push_str(&format!(
                r#"<tr><td><span class="badge badge--{tone}">{}</span> <code>{}</code><br>{}{extra}</td><td class="small">{}</td><td class="small"><code>{}</code> → <code>{} {}</code></td></tr>"#,
                o.method,
                escape_html(o.path),
                escape_html(o.summary),
                escape_html(&access_text(o.access)),
                o.request.unwrap_or("—"),
                o.status,
                escape_html(o.response),
            ));
        }
        body.push_str("</tbody></table></div></div></section>");
    }
    let css = state.assets.css_url();
    Html(format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>HostingOS API reference</title>
<link rel="stylesheet" href="{css}">
</head>
<body>
<main class="container stack-lg mt-8 mb-8">
<header class="page-head"><div><h1>HostingOS API v1</h1>
<p>REST + JSON on the tenant host. Machine-readable description: <a href="/api/v1/openapi.json"><code>/api/v1/openapi.json</code></a> (OpenAPI 3.1).</p></div></header>
<section class="card"><div class="card__body stack">
<h2>Basics</h2>
<ul>
<li><strong>Auth:</strong> <code>Authorization: Bearer hos_live_…</code>. Staff keys act with the user's permissions (∩ key scopes); client keys only see their own records — other ids answer <code>404</code>. Browser sessions work too (send <code>X-CSRF-Token</code> on unsafe methods).</li>
<li><strong>Errors:</strong> <code>application/problem+json</code> with <code>code</code>, <code>detail</code> and, for <code>422</code>, an <code>errors</code> array of <code>{{field, message}}</code>.</li>
<li><strong>Lists:</strong> <code>?limit=25&amp;offset=0&amp;sort=-created_at</code>; responses carry <code>pagination.total</code>, <code>has_more</code>, <code>next_offset</code>.</li>
<li><strong>Money:</strong> decimal strings (<code>"19.90"</code>) next to an ISO 4217 <code>currency</code>.</li>
<li><strong>Idempotency:</strong> send <code>Idempotency-Key: &lt;uuid&gt;</code> on POST; retries within 24 h replay the first response (<code>Idempotent-Replayed: true</code>).</li>
<li><strong>Rate limits:</strong> per key; see <code>RateLimit-Limit</code>, <code>RateLimit-Remaining</code>, <code>RateLimit-Reset</code> and <code>Retry-After</code> on <code>429</code>.</li>
<li><strong>Outgoing webhooks:</strong> verify <code>X-HostingOS-Signature: t=&lt;unix&gt;,v1=&lt;hex&gt;</code> = HMAC-SHA256(secret, <code>"&lt;t&gt;.&lt;raw body&gt;"</code>).</li>
</ul>
<nav aria-label="Sections"><ul class="cluster">{nav}</ul></nav>
</div></section>
{body}
</main>
</body>
</html>"#
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn document_covers_exactly_the_router() {
        let routes: BTreeSet<(String, String)> = super::super::route_table()
            .into_iter()
            .map(|(m, p)| (m.to_owned(), p.to_owned()))
            .collect();
        let documented: BTreeSet<(String, String)> = OPERATIONS
            .iter()
            .map(|o| (o.method.to_owned(), o.path.to_owned()))
            .collect();
        assert_eq!(routes, documented);
        let doc = document();
        assert_eq!(doc["openapi"], "3.1.0");
        let paths = doc["paths"].as_object().unwrap();
        for (m, p) in &routes {
            assert!(
                paths
                    .get(p)
                    .and_then(|v| v.get(m.to_ascii_lowercase()))
                    .is_some(),
                "{m} {p} missing in document"
            );
        }
    }

    #[test]
    fn every_schema_reference_resolves() {
        let doc = document();
        let text = doc.to_string();
        let schemas = doc["components"]["schemas"].as_object().unwrap();
        let params = doc["components"]["parameters"].as_object().unwrap();
        let responses = doc["components"]["responses"].as_object().unwrap();
        for part in text.split("\"$ref\":\"#/components/").skip(1) {
            let target = part.split('"').next().unwrap();
            let (kind, name) = target.split_once('/').unwrap();
            let ok = match kind {
                "schemas" => schemas.contains_key(name),
                "parameters" => params.contains_key(name),
                "responses" => responses.contains_key(name),
                _ => false,
            };
            assert!(ok, "dangling $ref {target}");
        }
        let ids: BTreeSet<String> = OPERATIONS.iter().map(operation_id).collect();
        assert_eq!(ids.len(), OPERATIONS.len(), "operationIds are unique");
    }
}
