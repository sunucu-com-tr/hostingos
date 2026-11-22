//! WHMCS API connector (`includes/api.php`, API credentials of a WHMCS
//! admin role). Responses are translated into rows with the WHMCS
//! **database column names**, so the engine's mappers are shared.
//!
//! What the API cannot bring (reported per entity through
//! [`Source::supports`]): client / user / staff **password hashes**
//! (clients and users must reset their password), encrypted server and
//! registrar credentials, tax rules, client groups, client notes,
//! configurable option groups and product addons (catalog), knowledge
//! base, product group names, domain nameservers.
//!
//! All requests go through the outbound policy (`HOS_OUTBOUND_POLICY`):
//! the DNS resolver refuses forbidden addresses, redirects are not
//! followed, bodies are size-capped and `https` is required.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use hos_core::net::{MAX_API_BODY, OutboundPolicy, outbound_error_in, read_body_limited};
use serde_json::Value as Json;

use super::{Attachment, AttachmentOwner, Batch, Flavor, Probe, SETTING_KEYS, Source, Support};
use crate::config::{ApiSourceConfig, SourceSecrets};
use crate::entity::Entity;
use crate::error::{ImportError, ImportResult};
use crate::map::DOMAIN_PRICE_COLUMNS;
use crate::row::{Row, Value};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Rows per page for actions that need one extra call per row.
const DETAIL_PAGE: u32 = 25;

/// Low-level WHMCS API client.
pub struct ApiClient {
    http: reqwest::Client,
    endpoint: url::Url,
    policy: OutboundPolicy,
    identifier: String,
    secret: String,
    access_key: Option<String>,
}

impl std::fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiClient")
            .field("endpoint", &self.endpoint.as_str())
            .finish_non_exhaustive()
    }
}

/// `…/includes/api.php` of a WHMCS system URL.
pub fn endpoint_of(url: &str, policy: OutboundPolicy) -> ImportResult<url::Url> {
    let mut u = url::Url::parse(url.trim())
        .map_err(|_| ImportError::Config("the WHMCS URL is not a valid URL".into()))?;
    if u.scheme() != "https" && !(u.scheme() == "http" && policy == OutboundPolicy::Unrestricted) {
        return Err(ImportError::Config("the WHMCS URL must use https".into()));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(ImportError::Config(
            "the WHMCS URL must not contain credentials".into(),
        ));
    }
    u.set_query(None);
    u.set_fragment(None);
    if !u.path().ends_with("/api.php") {
        let base = if u.path().ends_with('/') {
            u.path().to_owned()
        } else {
            format!("{}/", u.path())
        };
        u.set_path(&format!("{base}includes/api.php"));
    }
    policy.check_url(&u)?;
    Ok(u)
}

/// Items of a WHMCS list (`{"clients":{"client":[…]}}`): arrays, single
/// objects and empty strings are all handled.
pub fn items<'a>(v: &'a Json, path: &[&str]) -> Vec<&'a Json> {
    let mut cur = v;
    for k in path {
        match cur.get(k) {
            Some(next) => cur = next,
            None => return Vec::new(),
        }
    }
    match cur {
        Json::Array(a) => a.iter().collect(),
        Json::Object(_) => vec![cur],
        _ => Vec::new(),
    }
}

/// A row from a JSON object (scalars kept, nested values as JSON text) with
/// `renames` (`api key` → `column`) applied.
pub fn row_of(obj: &Json, renames: &[(&str, &str)]) -> Row {
    let mut r = Row::new();
    if let Some(map) = obj.as_object() {
        for (k, v) in map {
            r.set(k, Value::from(v));
        }
        for (from, to) in renames {
            if let Some(v) = map.get(*from) {
                r.set(to, Value::from(v));
            }
        }
    }
    r
}

fn total(v: &Json) -> Option<i64> {
    v.get("totalresults")
        .and_then(|t| t.as_i64().or_else(|| t.as_str()?.parse().ok()))
}

impl ApiClient {
    /// Client for `config`; checks the URL against `policy`.
    pub fn new(
        config: &ApiSourceConfig,
        secrets: &SourceSecrets,
        policy: OutboundPolicy,
    ) -> ImportResult<Self> {
        let endpoint = endpoint_of(&config.url, policy)?;
        let secret = secrets
            .api_secret
            .clone()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ImportError::Config("the API secret is required".into()))?;
        if config.identifier.trim().is_empty() {
            return Err(ImportError::Config("the API identifier is required".into()));
        }
        let http = policy
            .client_builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent("HostingOS-import/1")
            .build()
            .map_err(|e| ImportError::Connect(e.to_string()))?;
        Ok(Self {
            http,
            endpoint,
            policy,
            identifier: config.identifier.trim().to_owned(),
            secret,
            access_key: secrets
                .api_access_key
                .clone()
                .filter(|s| !s.trim().is_empty()),
        })
    }

    async fn call_limited(
        &self,
        action: &str,
        params: &[(&str, String)],
        limit: usize,
    ) -> ImportResult<Json> {
        self.policy.validate_url(&self.endpoint).await?;
        let mut form: Vec<(&str, String)> = vec![
            ("action", action.to_owned()),
            ("identifier", self.identifier.clone()),
            ("secret", self.secret.clone()),
            ("responsetype", "json".to_owned()),
        ];
        if let Some(k) = &self.access_key {
            form.push(("accesskey", k.clone()));
        }
        form.extend(params.iter().cloned());
        let resp = self
            .http
            .post(self.endpoint.clone())
            .form(&form)
            .send()
            .await
            .map_err(|e| match outbound_error_in(&e) {
                Some(p) => ImportError::Policy(p.clone()),
                None if e.is_timeout() => {
                    ImportError::Connect(format!("{action}: request timed out"))
                }
                None => ImportError::Connect(format!("{action}: {}", e.without_url())),
            })?;
        let status = resp.status();
        let body = read_body_limited(resp, limit)
            .await
            .map_err(|e| ImportError::Source(format!("{action}: {e}")))?;
        let json: Json = serde_json::from_slice(&body).map_err(|_| {
            ImportError::Source(format!(
                "{action}: HTTP {} with a non-JSON body — is this the WHMCS URL?",
                status.as_u16()
            ))
        })?;
        if json.get("result").and_then(Json::as_str) == Some("error") {
            let msg: String = json
                .get("message")
                .and_then(Json::as_str)
                .unwrap_or("unknown error")
                .chars()
                .take(300)
                .collect();
            let lower = msg.to_ascii_lowercase();
            return Err(
                if lower.contains("authentication")
                    || lower.contains("invalid ip")
                    || lower.contains("permission")
                {
                    ImportError::Auth(format!("{action}: {msg}"))
                } else {
                    ImportError::Source(format!("{action}: {msg}"))
                },
            );
        }
        if !status.is_success() {
            return Err(ImportError::Source(format!(
                "{action}: HTTP {}",
                status.as_u16()
            )));
        }
        Ok(json)
    }

    /// One API call.
    pub async fn call(&self, action: &str, params: &[(&str, String)]) -> ImportResult<Json> {
        self.call_limited(action, params, MAX_API_BODY).await
    }

    /// A ticket attachment through `GetTicketAttachment`.
    pub async fn attachment(
        &mut self,
        owner: AttachmentOwner,
        owner_id: i64,
        index: usize,
        max_bytes: u64,
    ) -> ImportResult<Attachment> {
        use base64::Engine as _;
        let limit =
            usize::try_from(max_bytes.saturating_mul(4) / 3 + 64 * 1024).unwrap_or(usize::MAX);
        let r = self
            .call_limited(
                "GetTicketAttachment",
                &[
                    ("relatedid", owner_id.to_string()),
                    ("type", owner.api_type().to_owned()),
                    ("index", index.to_string()),
                ],
                limit,
            )
            .await;
        let json = match r {
            Ok(j) => j,
            Err(ImportError::Source(m)) if m.contains("exceeds") => {
                return Ok(Attachment::TooLarge(None));
            }
            Err(ImportError::Source(m)) if m.to_ascii_lowercase().contains("not found") => {
                return Ok(Attachment::Missing);
            }
            Err(e) => return Err(e),
        };
        let Some(data) = json.get("data").and_then(Json::as_str) else {
            return Ok(Attachment::Missing);
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data.trim())
            .map_err(|_| ImportError::Source("GetTicketAttachment: data is not base64".into()))?;
        if bytes.len() as u64 > max_bytes {
            return Ok(Attachment::TooLarge(Some(bytes.len() as u64)));
        }
        Ok(Attachment::Data {
            filename: json
                .get("filename")
                .and_then(Json::as_str)
                .unwrap_or("attachment")
                .to_owned(),
            bytes,
        })
    }
}

/// The WHMCS API connector.
pub struct ApiSource {
    client: ApiClient,
    /// Whole-list responses of the entity being read (sliced by cursor).
    cache: Option<(Entity, Vec<Row>)>,
    currencies: Option<BTreeMap<String, i64>>,
}

impl ApiSource {
    /// Connector for `config`.
    pub fn new(
        config: &ApiSourceConfig,
        secrets: &SourceSecrets,
        policy: OutboundPolicy,
    ) -> ImportResult<Self> {
        Ok(Self {
            client: ApiClient::new(config, secrets, policy)?,
            cache: None,
            currencies: None,
        })
    }

    async fn currency_ids(&mut self) -> ImportResult<BTreeMap<String, i64>> {
        if let Some(c) = &self.currencies {
            return Ok(c.clone());
        }
        let j = self.client.call("GetCurrencies", &[]).await?;
        let map: BTreeMap<String, i64> = items(&j, &["currencies", "currency"])
            .into_iter()
            .filter_map(|c| {
                let r = row_of(c, &[]);
                Some((r.str("code")?.to_ascii_uppercase(), r.int("id")?))
            })
            .collect();
        self.currencies = Some(map.clone());
        Ok(map)
    }

    /// Rows of a non-paginated action, cached while the entity is read.
    async fn whole(&mut self, entity: Entity) -> ImportResult<Vec<Row>> {
        if let Some((e, rows)) = &self.cache
            && *e == entity
        {
            return Ok(rows.clone());
        }
        let rows = self.load_whole(entity).await?;
        self.cache = Some((entity, rows.clone()));
        Ok(rows)
    }

    async fn load_whole(&mut self, entity: Entity) -> ImportResult<Vec<Row>> {
        let c = &self.client;
        Ok(match entity {
            Entity::Currencies => {
                let j = c.call("GetCurrencies", &[]).await?;
                let mut rows: Vec<Row> = items(&j, &["currencies", "currency"])
                    .into_iter()
                    .map(|x| row_of(x, &[]))
                    .collect();
                // The API does not say which currency is the default:
                // WHMCS's default has exchange rate 1 (lowest id wins).
                rows.sort_by_key(Row::id);
                if let Some(i) = rows
                    .iter()
                    .position(|r| r.dec("rate") == Some(rust_decimal::Decimal::ONE))
                {
                    rows[i].set("default", 1);
                }
                rows
            }
            Entity::Servers => {
                let j = c.call("GetServers", &[]).await?;
                items(&j, &["servers"])
                    .into_iter()
                    .map(|x| row_of(x, &[("module", "type")]))
                    .collect()
            }
            Entity::Staff => {
                let j = c.call("GetAdminUsers", &[]).await?;
                items(&j, &["admin_users"])
                    .into_iter()
                    .map(|x| row_of(x, &[("roleId", "roleid"), ("isDisabled", "disabled")]))
                    .collect()
            }
            Entity::ProductGroups => {
                let products = self.products().await?;
                let gids: BTreeSet<i64> =
                    products.iter().filter_map(|p| p.reference("gid")).collect();
                gids.into_iter()
                    .map(|g| {
                        Row::new()
                            .with("id", g)
                            .with("name", format!("WHMCS group {g}"))
                            .with("order", g)
                    })
                    .collect()
            }
            Entity::Products => self.products().await?,
            Entity::Pricing => self.pricing().await?,
            Entity::Tlds => {
                let ids = self.currency_ids().await?;
                let first = ids.values().min().copied().unwrap_or(1);
                let j = self
                    .client
                    .call("GetTLDPricing", &[("currencyid", first.to_string())])
                    .await?;
                let mut rows = Vec::new();
                if let Some(p) = j.get("pricing").and_then(Json::as_object) {
                    for (i, (tld, v)) in p.iter().enumerate() {
                        let addons = v.get("addons").cloned().unwrap_or(Json::Null);
                        let flag = |k: &str| addons.get(k).and_then(Json::as_bool).unwrap_or(false);
                        let days = |k: &str| {
                            v.get(k)
                                .and_then(|g| g.get("days"))
                                .map(Value::from)
                                .unwrap_or(Value::Null)
                        };
                        rows.push(
                            Row::new()
                                .with("id", i64::try_from(i + 1).unwrap_or(0))
                                .with("extension", format!(".{}", tld.trim_start_matches('.')))
                                .with("dnsmanagement", flag("dns"))
                                .with("emailforwarding", flag("email"))
                                .with("idprotection", flag("idprotect"))
                                .with("grace_period", days("grace_period"))
                                .with("redemption_grace_period", days("redemption_period"))
                                .with("order", i64::try_from(i).unwrap_or(0)),
                        );
                    }
                }
                rows
            }
            Entity::Promotions => {
                let j = c.call("GetPromotions", &[]).await?;
                items(&j, &["promotions", "promotion"])
                    .into_iter()
                    .map(|x| row_of(x, &[]))
                    .collect()
            }
            Entity::ServiceAddons => {
                let j = c.call("GetClientsAddons", &[]).await?;
                items(&j, &["addons", "addon"])
                    .into_iter()
                    .map(|x| row_of(x, &[("serviceid", "hostingid"), ("recurring", "recurring")]))
                    .collect()
            }
            Entity::Transactions => {
                let j = c.call("GetTransactions", &[]).await?;
                items(&j, &["transactions", "transaction"])
                    .into_iter()
                    .map(|x| row_of(x, &[]))
                    .collect()
            }
            Entity::Departments => {
                let j = c
                    .call(
                        "GetSupportDepartments",
                        &[("ignore_dept_assignments", "true".into())],
                    )
                    .await?;
                items(&j, &["departments", "department"])
                    .into_iter()
                    .map(|x| row_of(x, &[]))
                    .collect()
            }
            Entity::CannedResponses => {
                let j = c.call("GetTicketPredefinedReplies", &[]).await?;
                items(&j, &["predefinedreplies", "predefinedreply"])
                    .into_iter()
                    .enumerate()
                    .map(|(i, x)| row_of(x, &[]).with("id", i64::try_from(i + 1).unwrap_or(0)))
                    .collect()
            }
            _ => Vec::new(),
        })
    }

    async fn products(&mut self) -> ImportResult<Vec<Row>> {
        let j = self.client.call("GetProducts", &[]).await?;
        Ok(items(&j, &["products", "product"])
            .into_iter()
            .map(|x| row_of(x, &[("pid", "id"), ("module", "servertype")]))
            .collect())
    }

    async fn pricing(&mut self) -> ImportResult<Vec<Row>> {
        let ids = self.currency_ids().await?;
        let mut rows = Vec::new();
        let j = self.client.call("GetProducts", &[]).await?;
        for p in items(&j, &["products", "product"]) {
            let pid = row_of(p, &[]).int("pid").unwrap_or(0);
            if let Some(pricing) = p.get("pricing").and_then(Json::as_object) {
                for (code, prices) in pricing {
                    let Some(cid) = ids.get(&code.to_ascii_uppercase()) else {
                        continue;
                    };
                    rows.push(
                        row_of(prices, &[])
                            .with("type", "product")
                            .with("relid", pid)
                            .with("currency", *cid),
                    );
                }
            }
        }
        for (_, cid) in ids {
            let j = self
                .client
                .call("GetTLDPricing", &[("currencyid", cid.to_string())])
                .await?;
            let Some(p) = j.get("pricing").and_then(Json::as_object) else {
                continue;
            };
            for (tld, v) in p {
                for (action, kind) in [
                    ("register", "domainregister"),
                    ("transfer", "domaintransfer"),
                    ("renew", "domainrenew"),
                ] {
                    let Some(years) = v.get(action).and_then(Json::as_object) else {
                        continue;
                    };
                    let mut r = Row::new()
                        .with("type", kind)
                        .with("tld", tld.trim_start_matches('.').to_ascii_lowercase())
                        .with("currency", cid);
                    for (y, col) in DOMAIN_PRICE_COLUMNS {
                        r.set(
                            col,
                            years
                                .get(&y.to_string())
                                .map_or(Value::Text("-1".into()), Value::from),
                        );
                    }
                    rows.push(r);
                }
            }
        }
        Ok(rows)
    }

    /// One page of a paginated list action.
    async fn page(
        &self,
        action: &str,
        extra: &[(&str, String)],
        path: &[&str],
        cursor: i64,
        limit: u32,
    ) -> ImportResult<(Vec<Json>, Option<i64>)> {
        let mut params: Vec<(&str, String)> = vec![
            ("limitstart", cursor.max(0).to_string()),
            ("limitnum", limit.to_string()),
        ];
        params.extend(extra.iter().cloned());
        let j = self.client.call(action, &params).await?;
        let list: Vec<Json> = items(&j, path).into_iter().cloned().collect();
        let end = cursor.max(0) + i64::try_from(list.len()).unwrap_or(0);
        let more = list.len() >= limit as usize && total(&j).is_none_or(|t| end < t);
        Ok((list, more.then_some(end)))
    }

    async fn clients_page(&mut self, cursor: i64, limit: u32) -> ImportResult<Batch> {
        let (list, next) = self
            .page(
                "GetClients",
                &[],
                &["clients", "client"],
                cursor,
                limit.min(DETAIL_PAGE),
            )
            .await?;
        let mut rows = Vec::with_capacity(list.len());
        for c in list {
            let id = row_of(&c, &[]).int("id").unwrap_or(0);
            let d = self
                .client
                .call(
                    "GetClientsDetails",
                    &[("clientid", id.to_string()), ("stats", "false".into())],
                )
                .await?;
            let detail = d.get("client").unwrap_or(&d);
            let mut r = row_of(
                detail,
                &[
                    ("countrycode", "country"),
                    ("tax_id", "tax_id"),
                    ("currency", "currency"),
                    ("defaultgateway", "defaultgateway"),
                ],
            );
            r.set("id", id);
            for k in [
                "datecreated",
                "status",
                "groupid",
                "email",
                "firstname",
                "lastname",
                "companyname",
            ] {
                if !r.has(k)
                    && let Some(v) = c.get(k)
                {
                    r.set(k, Value::from(v));
                }
            }
            if let Some(cf) = detail.get("customfields").or_else(|| d.get("customfields")) {
                r.set("customfields_json", cf.to_string());
            }
            rows.push(r);
        }
        Ok(Batch { rows, next })
    }

    async fn services_page(
        &mut self,
        entity: Entity,
        cursor: i64,
        limit: u32,
    ) -> ImportResult<Batch> {
        let (list, next) = self
            .page(
                "GetClientsProducts",
                &[],
                &["products", "product"],
                cursor,
                limit,
            )
            .await?;
        let mut rows = Vec::new();
        for p in &list {
            let svc = row_of(
                p,
                &[
                    ("clientid", "userid"),
                    ("pid", "packageid"),
                    ("serverid", "server"),
                    ("suspensionreason", "suspendreason"),
                    ("recurringamount", "amount"),
                    ("status", "domainstatus"),
                    ("serverhostname", "server_hostname"),
                ],
            );
            let sid = svc.id();
            match entity {
                Entity::Services => rows.push(svc),
                Entity::ServiceOptions => {
                    for (i, o) in items(p, &["configoptions", "configoption"])
                        .into_iter()
                        .enumerate()
                    {
                        let o = row_of(o, &[]);
                        let qty = (o.text("type").eq_ignore_ascii_case("quantity"))
                            .then(|| o.int("value").unwrap_or(0));
                        rows.push(
                            Row::new()
                                .with("id", sid * 1000 + i64::try_from(i).unwrap_or(0))
                                .with("relid", sid)
                                .with("configid", o.int("id").unwrap_or(0))
                                .with("opt_name", o.text("option"))
                                .with("opt_type", o.text("type"))
                                .with("sub_name", o.text("value"))
                                .with("qty", qty),
                        );
                    }
                }
                _ => {
                    for f in items(p, &["customfields", "customfield"]) {
                        let f = row_of(f, &[]);
                        rows.push(
                            Row::new()
                                .with("fieldid", f.int("id").unwrap_or(0))
                                .with("relid", sid)
                                .with("cf_type", "product")
                                .with(
                                    "cf_name",
                                    f.str("name").unwrap_or_else(|| {
                                        format!("field_{}", f.int("id").unwrap_or(0))
                                    }),
                                )
                                .with("value", f.raw_str("value")),
                        );
                    }
                }
            }
        }
        Ok(Batch { rows, next })
    }

    async fn ticket_detail(&self, id: i64) -> ImportResult<Json> {
        self.client
            .call("GetTicket", &[("ticketid", id.to_string())])
            .await
    }

    async fn tickets_page(
        &mut self,
        entity: Entity,
        cursor: i64,
        limit: u32,
    ) -> ImportResult<Batch> {
        let (list, next) = self
            .page(
                "GetTickets",
                &[("ignore_dept_assignments", "true".into())],
                &["tickets", "ticket"],
                cursor,
                limit.min(DETAIL_PAGE),
            )
            .await?;
        let mut rows = Vec::new();
        for t in &list {
            let base = row_of(t, &[("priority", "urgency")]);
            let tid = base.id();
            let d = self.ticket_detail(tid).await?;
            let replies = items(&d, &["replies", "reply"]);
            match entity {
                Entity::Tickets => {
                    let mut r = base;
                    if let Some(first) = replies
                        .iter()
                        .find(|x| row_of(x, &[]).int("replyid") == Some(0))
                    {
                        let f = row_of(first, &[]);
                        r.set("message", f.raw_str("message"));
                        r.set("attachment", f.raw_str("attachment"));
                    }
                    rows.push(r);
                }
                Entity::TicketReplies => {
                    for x in replies {
                        let x = row_of(x, &[("replyid", "id")]);
                        if x.id() > 0 {
                            rows.push(x.with("tid", tid));
                        }
                    }
                }
                _ => {
                    for n in items(&d, &["notes", "note"]) {
                        rows.push(row_of(n, &[("noteid", "id")]).with("ticketid", tid));
                    }
                }
            }
        }
        Ok(Batch { rows, next })
    }

    async fn invoice_items_page(&mut self, cursor: i64, limit: u32) -> ImportResult<Batch> {
        let (list, next) = self
            .page(
                "GetInvoices",
                &[],
                &["invoices", "invoice"],
                cursor,
                limit.min(DETAIL_PAGE),
            )
            .await?;
        let mut rows = Vec::new();
        for inv in &list {
            let id = row_of(inv, &[]).id();
            let d = self
                .client
                .call("GetInvoice", &[("invoiceid", id.to_string())])
                .await?;
            let userid = row_of(&d, &[]).int("userid").unwrap_or(0);
            for it in items(&d, &["items", "item"]) {
                rows.push(row_of(it, &[]).with("invoiceid", id).with("userid", userid));
            }
        }
        Ok(Batch { rows, next })
    }

    async fn credits_page(&mut self, cursor: i64, limit: u32) -> ImportResult<Batch> {
        let (list, next) = self
            .page(
                "GetClients",
                &[],
                &["clients", "client"],
                cursor,
                limit.min(DETAIL_PAGE),
            )
            .await?;
        let mut rows = Vec::new();
        for c in &list {
            let id = row_of(c, &[]).id();
            let j = self
                .client
                .call("GetCredits", &[("clientid", id.to_string())])
                .await?;
            for x in items(&j, &["credits", "credit"]) {
                rows.push(row_of(x, &[]).with("clientid", id));
            }
        }
        Ok(Batch { rows, next })
    }

    async fn user_links_page(&mut self, cursor: i64, limit: u32) -> ImportResult<Batch> {
        let (list, next) = self
            .page("GetUsers", &[], &["users"], cursor, limit)
            .await?;
        let mut rows = Vec::new();
        for u in &list {
            let uid = row_of(u, &[]).id();
            for c in items(u, &["clients"]) {
                let c = row_of(c, &[]);
                rows.push(
                    Row::new()
                        .with("id", uid * 100_000 + c.id())
                        .with("auth_user_id", uid)
                        .with("client_id", c.id())
                        .with("owner", c.flag("isOwner"))
                        .with("permissions", c.raw_str("permissions")),
                );
            }
        }
        Ok(Batch { rows, next })
    }

    fn slice(rows: Vec<Row>, cursor: i64, limit: u32) -> Batch {
        let start = usize::try_from(cursor.max(0)).unwrap_or(0);
        let end = (start + limit as usize).min(rows.len());
        let next = (end < rows.len()).then(|| i64::try_from(end).unwrap_or(i64::MAX));
        Batch {
            rows: rows
                .get(start..end)
                .map(<[Row]>::to_vec)
                .unwrap_or_default(),
            next,
        }
    }
}

/// A paginated list action: `(action, JSON path, key renames)`.
type PagedAction = (
    &'static str,
    &'static [&'static str],
    &'static [(&'static str, &'static str)],
);

/// Paged list actions per entity.
fn paged(entity: Entity) -> Option<PagedAction> {
    Some(match entity {
        Entity::Users => (
            "GetUsers",
            &["users"],
            &[
                ("firstname", "first_name"),
                ("lastname", "last_name"),
                ("datecreated", "created_at"),
            ],
        ),
        Entity::Contacts => ("GetContacts", &["contacts", "contact"], &[]),
        Entity::Invoices => ("GetInvoices", &["invoices", "invoice"], &[]),
        Entity::Orders => ("GetOrders", &["orders", "order"], &[]),
        Entity::Domains => (
            "GetClientsDomains",
            &["domains", "domain"],
            &[("domainname", "domain"), ("regtype", "type")],
        ),
        Entity::CancelRequests => ("GetCancelledPackages", &["packages", "package"], &[]),
        Entity::Announcements => ("GetAnnouncements", &["announcements", "announcement"], &[]),
        Entity::ActivityLog => ("GetActivityLog", &["activity", "entry"], &[]),
        _ => return None,
    })
}

#[async_trait]
impl Source for ApiSource {
    fn flavor(&self) -> Flavor {
        Flavor::Api
    }

    fn supports(&self, entity: Entity) -> Support {
        match entity {
            Entity::TaxRules => Support::No("tax rules are not exposed by the WHMCS API"),
            Entity::ClientGroups => Support::No("client groups are not exposed by the WHMCS API"),
            Entity::ClientNotes => Support::No("client notes are not exposed by the WHMCS API"),
            Entity::ConfigGroups
            | Entity::ConfigLinks
            | Entity::ConfigOptions
            | Entity::ConfigValues => Support::No(
                "configurable option groups are not exposed by the WHMCS API (service selections are kept by name)",
            ),
            Entity::Addons => Support::No(
                "the addon catalog is not exposed by the WHMCS API (service addons are imported as custom addons)",
            ),
            Entity::KbCategories | Entity::KbArticles => {
                Support::No("the knowledge base is not exposed by the WHMCS API")
            }
            Entity::Quotes
            | Entity::QuoteItems
            | Entity::Affiliates
            | Entity::AffiliateAccounts
            | Entity::AffiliateHistory
            | Entity::AffiliateWithdrawals
            | Entity::Todos
            | Entity::BannedIps
            | Entity::BannedEmails
            | Entity::DownloadCategories
            | Entity::Downloads
            | Entity::NetworkIssues => Support::No(
                "imported by the database connector only (the API does not expose all of it: referral links, commission history, ban lists, downloads, network issues)",
            ),
            Entity::Clients | Entity::Users => Support::Partial(
                "password hashes are not exposed by the API: these logins need a password reset",
            ),
            Entity::Staff => {
                Support::Partial("staff passwords are never imported; staff set a new password")
            }
            Entity::Servers => Support::Partial(
                "server credentials are not exposed by the API; integrations are created without them",
            ),
            Entity::ProductGroups => Support::Partial(
                "group names are not exposed by the API; groups are named after their WHMCS id",
            ),
            Entity::Domains => Support::Partial("nameservers are not exposed by the API list"),
            Entity::Services => {
                Support::Partial("next invoice dates are not exposed; the next due date is used")
            }
            _ => Support::Full,
        }
    }

    async fn probe(&mut self) -> ImportResult<Probe> {
        let j = self.client.call("WhmcsDetails", &[]).await?;
        let version = j
            .get("whmcs")
            .and_then(|w| w.get("version"))
            .and_then(Json::as_str)
            .map(str::to_owned);
        let mut notes = vec![format!(
            "API endpoint {}",
            self.client.endpoint.host_str().unwrap_or_default()
        )];
        // Cheap permission checks on the actions the import needs most.
        for action in ["GetClients", "GetInvoices", "GetTickets"] {
            if let Err(e) = self.client.call(action, &[("limitnum", "1".into())]).await {
                notes.push(format!("{action}: {e}"));
            }
        }
        Ok(Probe { version, notes })
    }

    async fn count(&mut self, entity: Entity) -> ImportResult<Option<u64>> {
        if matches!(self.supports(entity), Support::No(_)) {
            return Ok(Some(0));
        }
        let action = match entity {
            Entity::Clients | Entity::Credits => Some(("GetClients", vec![])),
            Entity::Invoices | Entity::InvoiceItems => Some(("GetInvoices", vec![])),
            Entity::Tickets | Entity::TicketReplies | Entity::TicketNotes => Some((
                "GetTickets",
                vec![("ignore_dept_assignments", "true".to_owned())],
            )),
            Entity::Services | Entity::ServiceOptions | Entity::CustomFieldValues => {
                Some(("GetClientsProducts", vec![]))
            }
            _ => paged(entity).map(|(a, _, _)| (a, vec![])),
        };
        if let Some((action, mut extra)) = action {
            extra.push(("limitnum", "1".to_owned()));
            let j = self.client.call(action, &extra).await?;
            // Child rows (replies, items, options) are counted by parent.
            let exact = matches!(
                entity,
                Entity::Clients
                    | Entity::Invoices
                    | Entity::Tickets
                    | Entity::Services
                    | Entity::Users
            ) || paged(entity).is_some();
            return Ok(total(&j)
                .filter(|_| exact)
                .map(|t| u64::try_from(t).unwrap_or(0)));
        }
        let rows = self.whole(entity).await?;
        Ok(Some(rows.len() as u64))
    }

    async fn count_table(&mut self, _table: &str) -> ImportResult<Option<u64>> {
        Ok(None)
    }

    async fn fetch(&mut self, entity: Entity, cursor: i64, limit: u32) -> ImportResult<Batch> {
        if matches!(self.supports(entity), Support::No(_)) {
            return Ok(Batch::default());
        }
        if self.cache.as_ref().is_some_and(|(e, _)| *e != entity) {
            self.cache = None;
        }
        let limit = limit.clamp(1, 250);
        match entity {
            Entity::Clients => self.clients_page(cursor, limit).await,
            Entity::Credits => self.credits_page(cursor, limit).await,
            Entity::UserLinks => self.user_links_page(cursor, limit).await,
            Entity::Services | Entity::ServiceOptions | Entity::CustomFieldValues => {
                self.services_page(entity, cursor, limit).await
            }
            Entity::Tickets | Entity::TicketReplies | Entity::TicketNotes => {
                self.tickets_page(entity, cursor, limit).await
            }
            Entity::InvoiceItems => self.invoice_items_page(cursor, limit).await,
            _ => {
                if let Some((action, path, renames)) = paged(entity) {
                    let (list, next) = self.page(action, &[], path, cursor, limit).await?;
                    return Ok(Batch {
                        rows: list.iter().map(|x| row_of(x, renames)).collect(),
                        next,
                    });
                }
                let rows = self.whole(entity).await?;
                Ok(Self::slice(rows, cursor, limit))
            }
        }
    }

    async fn settings(&mut self) -> ImportResult<BTreeMap<String, String>> {
        // `GetConfigurationValue` needs WHMCS 7.x+; missing settings fall
        // back to the importer's defaults.
        let mut out = BTreeMap::new();
        for key in SETTING_KEYS {
            if let Ok(j) = self
                .client
                .call("GetConfigurationValue", &[("setting", (*key).to_owned())])
                .await
                && let Some(v) = j.get("value")
            {
                out.insert(
                    (*key).to_owned(),
                    row_of(&serde_json::json!({ "v": v }), &[]).text("v"),
                );
            }
        }
        if !out.contains_key("Version")
            && let Ok(j) = self.client.call("WhmcsDetails", &[]).await
            && let Some(v) = j
                .get("whmcs")
                .and_then(|w| w.get("version"))
                .and_then(Json::as_str)
        {
            out.insert("Version".into(), v.to_owned());
        }
        Ok(out)
    }

    async fn attachment(
        &mut self,
        owner: AttachmentOwner,
        owner_id: i64,
        index: usize,
        _stored_name: &str,
        max_bytes: u64,
    ) -> ImportResult<Attachment> {
        self.client
            .attachment(owner, owner_id, index, max_bytes)
            .await
    }
}

/// Map of API key renames used by tests and docs (`api key → column`).
pub fn documented_renames() -> HashMap<&'static str, &'static str> {
    [
        ("GetClientsProducts.clientid", "tblhosting.userid"),
        ("GetClientsProducts.pid", "tblhosting.packageid"),
        ("GetClientsProducts.status", "tblhosting.domainstatus"),
        ("GetClientsProducts.recurringamount", "tblhosting.amount"),
        ("GetClientsDomains.domainname", "tbldomains.domain"),
        ("GetTickets.priority", "tbltickets.urgency"),
        ("GetProducts.pid", "tblproducts.id"),
    ]
    .into_iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoint_normalisation_and_policy() {
        let p = OutboundPolicy::Private;
        assert_eq!(
            endpoint_of("https://billing.example.com", p)
                .unwrap()
                .as_str(),
            "https://billing.example.com/includes/api.php"
        );
        assert_eq!(
            endpoint_of("https://example.com/whmcs/?x=1", p)
                .unwrap()
                .as_str(),
            "https://example.com/whmcs/includes/api.php"
        );
        assert_eq!(
            endpoint_of("https://example.com/includes/api.php", p)
                .unwrap()
                .as_str(),
            "https://example.com/includes/api.php"
        );
        assert!(matches!(
            endpoint_of("http://example.com", p),
            Err(ImportError::Config(_))
        ));
        assert!(matches!(
            endpoint_of("https://u:p@example.com", p),
            Err(ImportError::Config(_))
        ));
        assert!(matches!(
            endpoint_of("https://127.0.0.1/", p),
            Err(ImportError::Policy(_))
        ));
        assert!(matches!(
            endpoint_of("https://169.254.169.254/", p),
            Err(ImportError::Policy(_))
        ));
    }

    #[test]
    fn list_shapes() {
        let v = json!({"clients": {"client": [{"id": 1}, {"id": 2}]}});
        assert_eq!(items(&v, &["clients", "client"]).len(), 2);
        let single = json!({"clients": {"client": {"id": 1}}});
        assert_eq!(items(&single, &["clients", "client"]).len(), 1);
        let empty = json!({"clients": ""});
        assert!(items(&empty, &["clients", "client"]).is_empty());
        let r = row_of(
            &json!({"pid": "7", "name": "X", "nested": {"a": 1}}),
            &[("pid", "id")],
        );
        assert_eq!(r.id(), 7);
        assert_eq!(r.text("nested"), r#"{"a":1}"#);
    }
}
