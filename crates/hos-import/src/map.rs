//! Pure WHMCS → HostingOS value mappings (statuses, cycles, prices,
//! numbers, references). No I/O; every function is unit-tested.

use std::net::IpAddr;

use chrono::{Months, NaiveDate};
use rust_decimal::Decimal;

fn norm(s: &str) -> String {
    s.trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// WHMCS billing cycle → `billing_cycle` (`Free Account` / `One Time` → `one_time`).
pub fn billing_cycle(s: &str) -> Option<&'static str> {
    Some(match norm(s).as_str() {
        "monthly" => "monthly",
        "quarterly" => "quarterly",
        "semiannually" | "semiannual" => "semiannual",
        "annually" | "annual" | "yearly" => "annual",
        "biennially" | "biennial" => "biennial",
        "triennially" | "triennial" => "triennial",
        "onetime" | "freeaccount" | "free" => "one_time",
        _ => return None,
    })
}

/// Months of a cycle (`None` for one-time).
pub fn cycle_months(cycle: &str) -> Option<u32> {
    Some(match cycle {
        "monthly" => 1,
        "quarterly" => 3,
        "semiannual" => 6,
        "annual" => 12,
        "biennial" => 24,
        "triennial" => 36,
        _ => return None,
    })
}

/// Last day of a period starting at `start`.
pub fn period_end(start: NaiveDate, cycle: &str) -> Option<NaiveDate> {
    let months = cycle_months(cycle)?;
    start.checked_add_months(Months::new(months))?.pred_opt()
}

/// `tblclients.status` → `clients.status`.
pub fn client_status(s: &str) -> &'static str {
    match norm(s).as_str() {
        "inactive" => "inactive",
        "closed" => "closed",
        _ => "active",
    }
}

/// `tblhosting.domainstatus` / `tblhostingaddons.status` → service status.
/// `Fraud` → cancelled; `Completed` (a fulfilled one-off) → terminated.
pub fn service_status(s: &str) -> Option<&'static str> {
    Some(match norm(s).as_str() {
        "pending" => "pending",
        "active" => "active",
        "suspended" => "suspended",
        "terminated" | "completed" => "terminated",
        "cancelled" | "fraud" => "cancelled",
        _ => return None,
    })
}

/// `tbldomains.status` → `domains.status`.
pub fn domain_status(s: &str) -> Option<&'static str> {
    Some(match norm(s).as_str() {
        "pending" => "pending",
        "pendingregistration" => "pending_registration",
        "pendingtransfer" => "pending_transfer",
        "active" => "active",
        "grace" => "grace",
        "redemption" => "redemption",
        "expired" => "expired",
        "transferredaway" => "transferred_away",
        "cancelled" => "cancelled",
        "fraud" => "fraud",
        _ => return None,
    })
}

/// `tblinvoices.status` → `invoices.status`.
pub fn invoice_status(s: &str) -> Option<&'static str> {
    Some(match norm(s).as_str() {
        "draft" => "draft",
        "unpaid" | "overdue" => "unpaid",
        "paid" => "paid",
        "cancelled" => "cancelled",
        "refunded" => "refunded",
        "collections" => "collections",
        "paymentpending" => "payment_pending",
        _ => return None,
    })
}

/// `tblorders.status` → `orders.status` (`Active` = accepted).
pub fn order_status(s: &str) -> &'static str {
    match norm(s).as_str() {
        "active" | "accepted" | "completed" => "accepted",
        "cancelled" => "cancelled",
        "fraud" => "fraud",
        _ => "pending",
    }
}

/// `tbltickets.status` → `tickets.status`. Custom WHMCS statuses become `open`.
pub fn ticket_status(s: &str) -> (&'static str, bool) {
    match norm(s).as_str() {
        "open" => ("open", true),
        "answered" => ("answered", true),
        "customerreply" => ("customer_reply", true),
        "closed" => ("closed", true),
        "onhold" => ("on_hold", true),
        "inprogress" => ("in_progress", true),
        _ => ("open", false),
    }
}

/// `tbltickets.urgency` → `tickets.priority`.
pub fn ticket_priority(s: &str) -> &'static str {
    match norm(s).as_str() {
        "low" => "low",
        "high" => "high",
        "urgent" | "critical" => "urgent",
        _ => "medium",
    }
}

/// `tblproducts.type` (+ module) → `products.product_type`.
pub fn product_type(kind: &str, module: &str, name: &str) -> &'static str {
    let module = norm(module);
    match norm(kind).as_str() {
        "hostingaccount" => "shared_hosting",
        "reselleraccount" => "reseller_hosting",
        "server" => {
            if norm(name).contains("dedicated") {
                "dedicated"
            } else {
                "vps"
            }
        }
        _ if module.contains("ssl")
            || module.contains("digicert")
            || module.contains("sectigo") =>
        {
            "ssl"
        }
        _ if module.contains("license") || module.contains("licensing") => "license",
        _ if module.contains("email") || module.contains("mail") => "email",
        _ => "other",
    }
}

/// `tblproducts.paytype` → `pay_type`.
pub fn pay_type(s: &str) -> &'static str {
    match norm(s).as_str() {
        "free" => "free",
        "onetime" => "one_time",
        _ => "recurring",
    }
}

/// `tblproducts.autosetup` → `auto_setup`.
pub fn auto_setup(s: &str) -> &'static str {
    match norm(s).as_str() {
        "order" => "on_order",
        "payment" => "on_payment",
        "on" | "accept" => "on_accept",
        _ => "manual",
    }
}

/// `tblproductconfigoptions.optiontype` → `option_type`.
pub fn config_option_type(n: i64) -> &'static str {
    match n {
        2 => "radio",
        3 => "yes_no",
        4 => "quantity",
        _ => "dropdown",
    }
}

/// WHMCS `key|Friendly name` convention → `(key, label)`.
pub fn split_pipe(s: &str) -> (Option<String>, String) {
    match s.split_once('|') {
        Some((k, v)) if !k.trim().is_empty() => {
            let label = if v.trim().is_empty() {
                k.trim()
            } else {
                v.trim()
            };
            (Some(k.trim().to_owned()), label.to_owned())
        }
        _ => (None, s.trim().trim_matches('|').trim().to_owned()),
    }
}

/// Product / addon / config-option price columns of `tblpricing`:
/// `(cycle, price column, setup fee column)`.
pub const RECURRING_PRICE_COLUMNS: &[(&str, &str, &str)] = &[
    ("monthly", "monthly", "msetupfee"),
    ("quarterly", "quarterly", "qsetupfee"),
    ("semiannual", "semiannually", "ssetupfee"),
    ("annual", "annually", "asetupfee"),
    ("biennial", "biennially", "bsetupfee"),
    ("triennial", "triennially", "tsetupfee"),
];

/// Domain price columns of `tblpricing` (`domainregister|transfer|renew`): years 1–10.
pub const DOMAIN_PRICE_COLUMNS: &[(i16, &str)] = &[
    (1, "msetupfee"),
    (2, "qsetupfee"),
    (3, "ssetupfee"),
    (4, "asetupfee"),
    (5, "bsetupfee"),
    (6, "monthly"),
    (7, "quarterly"),
    (8, "semiannually"),
    (9, "annually"),
    (10, "biennially"),
];

/// A WHMCS price: negative (`-1.00`) = disabled.
pub fn price(v: Option<Decimal>) -> Option<Decimal> {
    v.filter(|p| *p >= Decimal::ZERO)
}

/// `tblpromotions.type` → `coupons.discount_type`.
pub fn discount_type(s: &str) -> Option<&'static str> {
    Some(match norm(s).as_str() {
        "percentage" => "percent",
        "fixedamount" => "fixed",
        "priceoverride" => "price_override",
        "freesetup" => "free_setup",
        _ => return None,
    })
}

/// What a promotion applies to (`tblpromotions.appliesto`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AppliesTo {
    pub products: Vec<i64>,
    pub addons: Vec<i64>,
    pub tlds: Vec<String>,
}

/// Parses `1,2,A3,D.com` (products, `A` addons, `D` TLDs).
pub fn applies_to(s: &str) -> AppliesTo {
    let mut out = AppliesTo::default();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some(a) = part.strip_prefix('A').and_then(|v| v.parse().ok()) {
            out.addons.push(a);
        } else if let Some(t) = part.strip_prefix('D') {
            let t = t.trim().trim_start_matches('.').to_ascii_lowercase();
            if !t.is_empty() {
                out.tlds.push(t);
            }
        } else if let Ok(p) = part.parse::<i64>() {
            out.products.push(p);
        }
    }
    out
}

/// Promotion cycles (`Monthly,Annually,1Years`) → HostingOS cycles (domain years are dropped).
pub fn promo_cycles(s: &str) -> Vec<String> {
    let mut out: Vec<String> = s
        .split(',')
        .filter_map(billing_cycle)
        .map(str::to_owned)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `tblinvoiceitems.type` → `invoice_items.item_type`.
pub fn invoice_item_type(s: &str) -> &'static str {
    match norm(s).as_str() {
        "hosting" => "service",
        "addon" => "addon",
        "domainregister" | "domaintransfer" | "domain" | "domainaddondns" | "domainaddonemf"
        | "domainaddonidp" => "domain",
        "setup" => "setup_fee",
        "latefee" => "late_fee",
        "promohosting" | "promodomain" | "groupdiscount" => "discount",
        "upgrade" => "upgrade",
        _ => "manual",
    }
}

/// Invoice number and numeric sequence: `invoicenum` when WHMCS assigned
/// one (sequential paid-invoice numbering), otherwise the invoice id, which
/// is what WHMCS shows.
pub fn invoice_number(invoicenum: Option<&str>, id: i64) -> (String, Option<i64>) {
    let number = invoicenum
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(|| id.to_string(), str::to_owned);
    let digits: String = number
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let seq = digits.parse::<i64>().ok().filter(|n| *n > 0);
    (number.chars().take(64).collect(), seq)
}

/// Two-letter country code (`tr` → `TR`); anything else → `fallback`.
pub fn country(code: &str, fallback: &str) -> String {
    let c = code.trim().to_ascii_uppercase();
    if c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase()) {
        c
    } else {
        fallback.to_owned()
    }
}

/// WHMCS language (`turkish`, `english`, `tr`) → `tr` | `en`.
pub fn locale(lang: &str, fallback: &str) -> String {
    match norm(lang).as_str() {
        "turkish" | "tr" | "trtr" | "turkce" => "tr".into(),
        "english" | "en" | "engb" | "enus" => "en".into(),
        _ => fallback.to_owned(),
    }
}

/// Normalised e-mail if it is plausible (`a@b.c`, no spaces, ≤ 254).
pub fn email(s: &str) -> Option<String> {
    let e = s.trim().to_ascii_lowercase();
    let (local, domain) = e.split_once('@')?;
    let ok = !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && e.len() <= 254
        && !e.contains(char::is_whitespace)
        && e.matches('@').count() == 1
        && !e.contains(['<', '>', '"', ',', ';']);
    ok.then_some(e)
}

/// IP addresses from a free-text list (newlines, commas, spaces), deduplicated.
pub fn ip_list(s: &str) -> Vec<IpAddr> {
    let mut out: Vec<IpAddr> = Vec::new();
    for part in s.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        if let Ok(ip) = part.trim().parse::<IpAddr>()
            && !out.contains(&ip)
        {
            out.push(ip);
        }
    }
    out
}

/// A host name usable as a nameserver.
pub fn nameserver(s: &str) -> Option<String> {
    let n = s.trim().trim_end_matches('.').to_ascii_lowercase();
    (n.len() >= 4
        && n.len() <= 253
        && n.contains('.')
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'))
    .then_some(n)
}

/// `tbltickets.service` (`S12` service, `D7` domain).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketRef {
    Service(i64),
    Domain(i64),
}

/// Parses `tbltickets.service`.
pub fn ticket_ref(s: &str) -> Option<TicketRef> {
    let s = s.trim();
    let (kind, id) = s.split_at(s.len().min(1));
    let id: i64 = id.parse().ok().filter(|i| *i > 0)?;
    match kind {
        "S" | "s" => Some(TicketRef::Service(id)),
        "D" | "d" => Some(TicketRef::Domain(id)),
        _ => None,
    }
}

/// Stored attachment names (`123456_report.pdf|987_x.png`) → `(stored, display)`.
pub fn attachment_names(s: &str) -> Vec<(String, String)> {
    s.split('|')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|stored| {
            let display = match stored.split_once('_') {
                Some((prefix, rest))
                    if !rest.is_empty() && prefix.bytes().all(|b| b.is_ascii_alphanumeric()) =>
                {
                    rest
                }
                _ => stored,
            };
            (stored.to_owned(), display.to_owned())
        })
        .collect()
}

/// WHMCS server module → `(integration category, provider kind)` of HostingOS.
pub fn server_kind(module: &str) -> Option<(&'static str, &'static str)> {
    let m = norm(module);
    Some(match m.as_str() {
        "cpanel" | "whm" | "cpanelwhm" => ("panel", "cpanel"),
        "plesk" | "pleskextended" => ("panel", "plesk"),
        "directadmin" | "directadminextended" => ("panel", "directadmin"),
        "cyberpanel" => ("panel", "cyberpanel"),
        "virtualizor" | "virtualizorcloud" => ("virt", "virtualizor"),
        "hetznercloud" | "hetzner" => ("virt", "hetzner"),
        "vmware" | "vcenter" | "vsphere" | "esxi" => ("virt", "vmware"),
        _ if m.contains("proxmox") || m.starts_with("pve") => ("virt", "proxmox"),
        _ => return None,
    })
}

/// WHMCS gateway module → the HostingOS gateway kind it corresponds to (for
/// `transactions.gateway` and mapping suggestions).
pub fn gateway_kind(module: &str) -> String {
    let m = norm(module);
    match m.as_str() {
        "paypal" | "paypalcheckout" | "paypalbilling" | "paypalpaymentspro" => "paypal".into(),
        "stripe" | "stripesepa" | "stripeach" => "stripe".into(),
        "banktransfer" | "bankpayment" | "havale" => "bank_transfer".into(),
        "iyzico" | "iyzipay" | "iyzicopay" => "iyzico".into(),
        "paytr" => "paytr".into(),
        "" => "manual".into(),
        other => other.chars().take(40).collect(),
    }
}

/// WHMCS registrar module → HostingOS registrar kind, when one exists.
pub fn registrar_kind(module: &str) -> Option<&'static str> {
    match norm(module).as_str() {
        "resellerclub" | "netearthone" | "resellercamp" => Some("resellerclub"),
        "namecheap" => Some("namecheap"),
        "openprovider" => Some("openprovider"),
        _ => None,
    }
}

/// WHMCS sub-account permissions (`profile,contacts,products,…`) → HostingOS
/// client member areas. An owner gets `[]` (everything); a member whose
/// WHMCS permissions map to nothing gets `["profile"]`, because an empty
/// list means "all areas" in HostingOS.
///
/// A HostingOS area is all-or-nothing (viewing and acting), so only WHMCS
/// rights that already include acting map to one: `manageproducts` →
/// `services`, `managedomains` → `domains`. The view-only `products` /
/// `domains` and the panel login `productsso` give no area (a viewer must
/// not become able to cancel, reinstall or transfer).
pub fn member_permissions(whmcs: &str, owner: bool) -> Vec<String> {
    if owner {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for p in whmcs.split(',').map(norm) {
        let area = match p.as_str() {
            "profile" => "profile",
            "contacts" => "contacts",
            "manageproducts" => "services",
            "managedomains" => "domains",
            "invoices" => "invoices",
            "orders" => "orders",
            "tickets" => "tickets",
            _ => continue,
        };
        if !out.iter().any(|a| a == area) {
            out.push(area.to_owned());
        }
    }
    if out.is_empty() {
        out.push("profile".into());
    }
    out
}

/// A `#rgb` / `#rrggbb` colour or `None`.
pub fn color(s: &str) -> Option<String> {
    let c = s.trim();
    let hex = c.strip_prefix('#')?;
    (matches!(hex.len(), 3 | 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| c.to_owned())
}

/// `tblquotes.stage` → `quotes.status` (Draft, Delivered, On Hold,
/// Accepted, Lost, Dead). An expired `sent` quote is turned into `expired`
/// by the caller.
pub fn quote_status(stage: &str) -> &'static str {
    match norm(stage).as_str() {
        "delivered" | "onhold" => "sent",
        "accepted" => "accepted",
        "lost" => "declined",
        "dead" => "cancelled",
        _ => "draft",
    }
}

/// A quote line amount: `quantity × unit price × (1 − discount %)`, rounded
/// half away from zero to cents (as quotes made in HostingOS); the discount
/// is clamped to 0–100.
pub fn quote_line(quantity: Decimal, unit_price: Decimal, discount: Decimal) -> (Decimal, Decimal) {
    let pct = discount.clamp(Decimal::ZERO, Decimal::ONE_HUNDRED);
    let amount = (quantity * unit_price * (Decimal::ONE_HUNDRED - pct) / Decimal::ONE_HUNDRED)
        .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::MidpointAwayFromZero)
        .max(Decimal::ZERO);
    (pct, amount)
}

/// `tblaffiliates.paytype` / `payamount` → commission override
/// (`None`: the programme default).
pub fn affiliate_commission(
    paytype: &str,
    amount: Option<Decimal>,
) -> Option<(&'static str, Decimal)> {
    let v = amount.filter(|a| *a >= Decimal::ZERO)?;
    match norm(paytype).as_str() {
        "percentage" | "percent" => Some(("percent", v.min(Decimal::ONE_HUNDRED))),
        "fixedamount" | "fixed" => Some(("fixed", v)),
        _ => None,
    }
}

/// A public referral code from the target id (`[A-Z0-9]`, 14 characters;
/// the random tail of a UUID v7).
pub fn affiliate_code(id: uuid::Uuid) -> String {
    let hex = id.simple().to_string().to_ascii_uppercase();
    format!("WH{}", &hex[20..])
}

/// `tbltodolist.status` → `(staff_tasks.status, done)`: Completed is done,
/// New / Pending / In Progress / anything else stays open.
pub fn todo_status(s: &str) -> &'static str {
    if norm(s) == "completed" {
        "done"
    } else {
        "open"
    }
}

/// A banned IP (`tblbannedips.ip`) as an address or CIDR network; a
/// trailing `*` wildcard (`10.1.2.*`) becomes the matching network.
pub fn ban_ip(raw: &str) -> Option<String> {
    let v = raw.trim().to_ascii_lowercase();
    if let Some(prefix) = v.strip_suffix(".*") {
        let parts: Vec<&str> = prefix.split('.').collect();
        if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.parse::<u8>().is_err()) {
            return None;
        }
        let mut octets = parts.clone();
        while octets.len() < 4 {
            octets.push("0");
        }
        let ip: std::net::Ipv4Addr = octets.join(".").parse().ok()?;
        return Some(format!("{ip}/{}", parts.len() * 8));
    }
    let (addr, prefix) = match v.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (v.as_str(), None),
    };
    let ip: IpAddr = addr.parse().ok()?;
    let max = if ip.is_ipv4() { 32 } else { 128 };
    match prefix {
        None => Some(ip.to_string()),
        Some(p) => {
            let p: u8 = p.parse().ok()?;
            (p <= max).then(|| format!("{ip}/{p}"))
        }
    }
}

/// A banned e-mail domain (`tblbannedemails.domain`): `@example.com`,
/// `example.com` → `example.com`; `None` when it is not a domain name.
pub fn ban_domain(raw: &str) -> Option<String> {
    let d = raw.trim().to_ascii_lowercase();
    let d = d
        .trim_start_matches('*')
        .trim_start_matches('@')
        .trim_end_matches('.');
    let ok = d.len() <= 253
        && d.contains('.')
        && d.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    ok.then(|| d.to_owned())
}

/// `tbldownloads` flags → `downloads.access`: a product download needs a
/// service of a linked product, "clients only" a signed-in client.
pub fn download_access(clients_only: bool, product_download: bool) -> &'static str {
    if product_download {
        "product"
    } else if clients_only {
        "clients"
    } else {
        "public"
    }
}

/// Content type offered for a download from its file name.
pub fn download_content_type(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map_or("", |(_, e)| e);
    match ext {
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "txt" | "log" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

/// A WHMCS network issue's status and priority → `(kind, status, impact)`
/// of `status_incidents`. "Scheduled" (or an issue with an end date that is
/// not an outage) becomes maintenance.
pub fn network_issue(
    status: &str,
    priority: &str,
    scheduled: bool,
) -> (&'static str, &'static str, &'static str) {
    let s = norm(status);
    let maintenance = s == "scheduled" || (scheduled && s != "outage" && s != "reported");
    let impact = match norm(priority).as_str() {
        "critical" => "critical",
        "high" => "major",
        "low" => "none",
        _ => "minor",
    };
    let status = match (maintenance, s.as_str()) {
        (true, "resolved") => "completed",
        (true, "inprogress" | "investigating" | "outage") => "in_progress",
        (true, _) => "scheduled",
        (false, "resolved") => "resolved",
        (false, "inprogress" | "outage") => "identified",
        (false, _) => "investigating",
    };
    (
        if maintenance {
            "maintenance"
        } else {
            "incident"
        },
        status,
        if maintenance && impact == "none" {
            "none"
        } else {
            impact
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn cycles() {
        assert_eq!(billing_cycle("Semi-Annually"), Some("semiannual"));
        assert_eq!(billing_cycle("Free Account"), Some("one_time"));
        assert_eq!(billing_cycle("One Time"), Some("one_time"));
        assert_eq!(billing_cycle("annually"), Some("annual"));
        assert_eq!(billing_cycle("weekly"), None);
        assert_eq!(
            period_end(d("2024-01-31"), "monthly"),
            Some(d("2024-02-28"))
        );
        assert_eq!(period_end(d("2024-03-01"), "annual"), Some(d("2025-02-28")));
        assert_eq!(period_end(d("2024-03-01"), "one_time"), None);
    }

    #[test]
    fn statuses() {
        assert_eq!(client_status("Closed"), "closed");
        assert_eq!(client_status("whatever"), "active");
        assert_eq!(service_status("Fraud"), Some("cancelled"));
        assert_eq!(service_status("Completed"), Some("terminated"));
        assert_eq!(service_status("Weird"), None);
        assert_eq!(domain_status("Pending Transfer"), Some("pending_transfer"));
        assert_eq!(domain_status("Transferred Away"), Some("transferred_away"));
        assert_eq!(invoice_status("Payment Pending"), Some("payment_pending"));
        assert_eq!(invoice_status("Collections"), Some("collections"));
        assert_eq!(order_status("Active"), "accepted");
        assert_eq!(order_status("Pending"), "pending");
        assert_eq!(ticket_status("Customer-Reply"), ("customer_reply", true));
        assert_eq!(ticket_status("Awaiting Parts"), ("open", false));
        assert_eq!(ticket_priority("High"), "high");
        assert_eq!(ticket_priority(""), "medium");
    }

    #[test]
    fn catalog_values() {
        assert_eq!(
            product_type("hostingaccount", "cpanel", "x"),
            "shared_hosting"
        );
        assert_eq!(product_type("reselleraccount", "", "x"), "reseller_hosting");
        assert_eq!(product_type("server", "", "Dedicated E3"), "dedicated");
        assert_eq!(product_type("server", "virtualizor", "VPS 1"), "vps");
        assert_eq!(product_type("other", "digicertssl", "SSL"), "ssl");
        assert_eq!(product_type("other", "", "Setup work"), "other");
        assert_eq!(pay_type("onetime"), "one_time");
        assert_eq!(pay_type("free"), "free");
        assert_eq!(pay_type("recurring"), "recurring");
        assert_eq!(auto_setup("payment"), "on_payment");
        assert_eq!(auto_setup(""), "manual");
        assert_eq!(config_option_type(4), "quantity");
        assert_eq!(config_option_type(9), "dropdown");
        assert_eq!(
            split_pipe("cpu|CPU Cores"),
            (Some("cpu".into()), "CPU Cores".into())
        );
        assert_eq!(split_pipe("Plain"), (None, "Plain".into()));
        assert_eq!(split_pipe("4|"), (Some("4".into()), "4".into()));
        assert_eq!(price(Some(dec!(-1))), None);
        assert_eq!(price(Some(dec!(0))), Some(dec!(0)));
        assert_eq!(price(None), None);
        assert_eq!(color("#abc"), Some("#abc".into()));
        assert_eq!(color("red"), None);
    }

    #[test]
    fn promotions() {
        assert_eq!(discount_type("Fixed Amount"), Some("fixed"));
        assert_eq!(discount_type("Bogus"), None);
        let a = applies_to("1, 2,A3,D.com,Dnet, junk");
        assert_eq!(a.products, vec![1, 2]);
        assert_eq!(a.addons, vec![3]);
        assert_eq!(a.tlds, vec!["com".to_owned(), "net".to_owned()]);
        assert_eq!(
            promo_cycles("Monthly,Annually,1Years,Monthly"),
            vec!["annual", "monthly"]
        );
    }

    #[test]
    fn invoices() {
        assert_eq!(invoice_item_type("Hosting"), "service");
        assert_eq!(invoice_item_type("DomainTransfer"), "domain");
        assert_eq!(invoice_item_type("PromoHosting"), "discount");
        assert_eq!(invoice_item_type("LateFee"), "late_fee");
        assert_eq!(invoice_item_type(""), "manual");
        assert_eq!(
            invoice_number(Some("INV-2024-0042"), 7),
            ("INV-2024-0042".into(), Some(42))
        );
        assert_eq!(invoice_number(Some(" "), 1234), ("1234".into(), Some(1234)));
        assert_eq!(invoice_number(None, 9), ("9".into(), Some(9)));
        assert_eq!(invoice_number(Some("ABC"), 9), ("ABC".into(), None));
    }

    #[test]
    fn people_and_network() {
        assert_eq!(country("tr", "US"), "TR");
        assert_eq!(country("Turkey", "US"), "US");
        assert_eq!(locale("Turkish", "en"), "tr");
        assert_eq!(locale("german", "en"), "en");
        assert_eq!(email(" Ada@Example.COM "), Some("ada@example.com".into()));
        for bad in [
            "",
            "a@b",
            "@x.com",
            "a b@x.com",
            "a@x.com,b@y.com",
            "<a@x.com>",
            "a@@x.com",
        ] {
            assert_eq!(email(bad), None, "{bad}");
        }
        assert_eq!(
            ip_list("1.2.3.4\n2001:db8::1, 1.2.3.4 junk"),
            vec![
                "1.2.3.4".parse::<IpAddr>().unwrap(),
                "2001:db8::1".parse().unwrap()
            ]
        );
        assert_eq!(
            nameserver("NS1.Example.com."),
            Some("ns1.example.com".into())
        );
        assert_eq!(nameserver("ns1"), None);
        assert_eq!(ticket_ref("S12"), Some(TicketRef::Service(12)));
        assert_eq!(ticket_ref("D7"), Some(TicketRef::Domain(7)));
        assert_eq!(ticket_ref("X1"), None);
        assert_eq!(ticket_ref(""), None);
        assert_eq!(
            attachment_names("123456_report.pdf|987_x_y.png||plain.txt"),
            vec![
                ("123456_report.pdf".into(), "report.pdf".into()),
                ("987_x_y.png".into(), "x_y.png".into()),
                ("plain.txt".into(), "plain.txt".into())
            ]
        );
    }

    #[test]
    fn modules() {
        assert_eq!(server_kind("cpanel"), Some(("panel", "cpanel")));
        assert_eq!(server_kind("DirectAdmin"), Some(("panel", "directadmin")));
        assert_eq!(server_kind("proxmoxVPS"), Some(("virt", "proxmox")));
        assert_eq!(server_kind("virtualizor"), Some(("virt", "virtualizor")));
        assert_eq!(server_kind("autorelease"), None);
        assert_eq!(gateway_kind("banktransfer"), "bank_transfer");
        assert_eq!(gateway_kind("PayPalCheckout"), "paypal");
        assert_eq!(gateway_kind("mollie"), "mollie");
        assert_eq!(gateway_kind(""), "manual");
        assert_eq!(registrar_kind("ResellerClub"), Some("resellerclub"));
        assert_eq!(registrar_kind("enom"), None);
    }

    #[test]
    fn quotes() {
        assert_eq!(quote_status("Draft"), "draft");
        assert_eq!(quote_status("Delivered"), "sent");
        assert_eq!(quote_status("On Hold"), "sent");
        assert_eq!(quote_status("Accepted"), "accepted");
        assert_eq!(quote_status("Lost"), "declined");
        assert_eq!(quote_status("Dead"), "cancelled");
        assert_eq!(quote_status("weird"), "draft");
        assert_eq!(
            quote_line(dec!(3), dec!(5.00), dec!(2.5)),
            (dec!(2.5), dec!(14.63))
        );
        assert_eq!(
            quote_line(dec!(1), dec!(10), dec!(150)),
            (dec!(100), dec!(0))
        );
        assert_eq!(
            quote_line(dec!(2), dec!(9.99), dec!(-5)),
            (dec!(0), dec!(19.98))
        );
    }

    #[test]
    fn affiliates() {
        assert_eq!(
            affiliate_commission("percentage", Some(dec!(15))),
            Some(("percent", dec!(15)))
        );
        assert_eq!(
            affiliate_commission("percentage", Some(dec!(150))),
            Some(("percent", dec!(100)))
        );
        assert_eq!(
            affiliate_commission("fixedamount", Some(dec!(7.5))),
            Some(("fixed", dec!(7.5)))
        );
        assert_eq!(affiliate_commission("", Some(dec!(10))), None);
        assert_eq!(affiliate_commission("fixedamount", Some(dec!(-1))), None);
        assert_eq!(affiliate_commission("fixedamount", None), None);
        let id = uuid::Uuid::now_v7();
        let code = affiliate_code(id);
        assert_eq!(code.len(), 14);
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        );
        assert_eq!(code, affiliate_code(id), "stable for a target id");
    }

    #[test]
    fn todos_and_bans() {
        assert_eq!(todo_status("Completed"), "done");
        assert_eq!(todo_status("In Progress"), "open");
        assert_eq!(todo_status("New"), "open");
        assert_eq!(ban_ip(" 192.0.2.7 ").as_deref(), Some("192.0.2.7"));
        assert_eq!(ban_ip("192.0.2.*").as_deref(), Some("192.0.2.0/24"));
        assert_eq!(ban_ip("10.*").as_deref(), Some("10.0.0.0/8"));
        assert_eq!(ban_ip("2001:DB8::1").as_deref(), Some("2001:db8::1"));
        assert_eq!(
            ban_ip("198.51.100.0/24").as_deref(),
            Some("198.51.100.0/24")
        );
        assert_eq!(ban_ip("198.51.100.0/33"), None);
        assert_eq!(ban_ip("300.1.1.*"), None);
        assert_eq!(ban_ip("not an ip"), None);
        assert_eq!(ban_domain("@Spam.Example").as_deref(), Some("spam.example"));
        assert_eq!(
            ban_domain("mail.example.com.").as_deref(),
            Some("mail.example.com")
        );
        assert_eq!(ban_domain("localhost"), None);
        assert_eq!(ban_domain("bad_domain.com"), None);
    }

    #[test]
    fn downloads_and_network_issues() {
        assert_eq!(download_access(false, false), "public");
        assert_eq!(download_access(true, false), "clients");
        assert_eq!(download_access(true, true), "product");
        assert_eq!(download_content_type("Guide.PDF"), "application/pdf");
        assert_eq!(
            download_content_type("setup.exe"),
            "application/octet-stream"
        );
        assert_eq!(
            network_issue("Reported", "High", false),
            ("incident", "investigating", "major")
        );
        assert_eq!(
            network_issue("Outage", "Critical", true),
            ("incident", "identified", "critical")
        );
        assert_eq!(
            network_issue("Resolved", "Medium", false),
            ("incident", "resolved", "minor")
        );
        assert_eq!(
            network_issue("Scheduled", "Low", true),
            ("maintenance", "scheduled", "none")
        );
        assert_eq!(
            network_issue("In Progress", "Low", true),
            ("maintenance", "in_progress", "none")
        );
        assert_eq!(
            network_issue("Resolved", "Medium", true),
            ("maintenance", "completed", "minor")
        );
    }

    #[test]
    fn member_areas() {
        assert!(member_permissions("anything", true).is_empty());
        assert_eq!(
            member_permissions("profile,products,manageproducts,invoices,quotes", false),
            vec!["profile", "services", "invoices"]
        );
        assert_eq!(member_permissions("affiliates", false), vec!["profile"]);
        assert_eq!(member_permissions("", false), vec!["profile"]);
        // View-only rights never widen to the acting area.
        assert_eq!(
            member_permissions("products,productsso,domains,tickets", false),
            vec!["tickets"]
        );
        assert_eq!(
            member_permissions("managedomains,manageproducts", false),
            vec!["domains", "services"]
        );
    }
}
