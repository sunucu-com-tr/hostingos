//! Paraşüt (pre-accounting service with built-in e-Fatura / e-Arşiv).
//!
//! * **Docs** (read 2026-11-18): Paraşüt API v4 reference
//!   <https://apidocs.parasut.com> (the site refuses automated fetches; read
//!   through the OpenAPI 3 artifacts generated from the official swagger —
//!   `ApiHome`, `Contacts`, `SalesInvoices`, `EArchives`, `EInvoices`,
//!   `EInvoiceInboxes`, `TrackableJobs` — and the official `parasutcom/api-doc`
//!   repository text: authentication, rate limit, trackable-job polling).
//! * **API version**: v4 (`https://api.parasut.com/v4/{company_id}/…`),
//!   JSON:API documents.
//! * **Auth**: OAuth 2 password grant at `POST /oauth/token`
//!   (`client_id`, `client_secret`, `username`, `password`,
//!   `redirect_uri=urn:ietf:wg:oauth:2.0:oob`), 2-hour bearer token renewed
//!   with the refresh token. Rate limit: 10 requests / 10 s.
//! * **Sandbox**: none documented; the base URL is configurable (Paraşüt
//!   hands out test companies on request).
//!
//! ## Flow (each step is recorded in the checkpoint, so a retry resumes)
//!
//! 1. find (by tax number / e-mail) or create the **contact**;
//! 2. find (by contact + date + description) or create the **sales invoice**
//!    with the composed lines, then compare Paraşüt's `net_total` with
//!    HostingOS's payable amount — on any difference the draft is deleted and
//!    nothing legal is created;
//! 3. unless the invoice already has an `active_e_document`, create the
//!    `e_archives` / `e_invoices` document (async **trackable job**);
//! 4. poll the job, then read the e-document (number, ETTN, cancellable-until).
//!
//! Paraşüt assigns the legal number and the ETTN. Withholding, return
//! invoices, UBL download and e-Arşiv cancellation are **not** used: the API
//! reference does not document their semantics for these resources, so
//! HostingOS reports them as unsupported and flags staff instead.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use hos_core::IntegrationError;
use hos_core::integrations::einvoice::{
    Capabilities, DocumentKind, DocumentState, DocumentStatus, EDocumentRef, EDocumentRequest,
    EInvoiceProvider, InvoiceTypeCode, PartyId, PaymentMethod, Profile, TaxpayerInfo,
};
use hos_core::integrations::{ConnectionInfo, FieldReader};
use hos_core::money::Currency;
use hos_core::net::OutboundPolicy;
use hos_core::secret::SecretString;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use url::Url;
use uuid::Uuid;

use crate::http::{HttpClient, MAX_API_BODY, MAX_DOCUMENT_BODY, RawResponse, describe_body};
use crate::ubl;

/// `integrations.kind`.
pub const KIND: &str = hos_core::integrations::kinds::PARASUT;
/// Production host.
pub const LIVE_URL: &str = "https://api.parasut.com/";

/// Settings of a Paraşüt integration.
#[derive(Debug, Clone)]
pub struct ParasutConfig {
    /// API host (`https://api.parasut.com/`).
    pub base_url: Url,
    /// Company (firma) id.
    pub company_id: String,
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret.
    pub client_secret: SecretString,
    /// Paraşüt user e-mail.
    pub username: String,
    /// Paraşüt user password.
    pub password: SecretString,
    /// e-Fatura scenario `commercial` (TICARIFATURA) instead of `basic`.
    pub commercial: bool,
    /// The company is a Paraşüt test company.
    pub test_company: bool,
    /// Request timeout.
    pub timeout: Duration,
    /// Outbound policy.
    pub outbound: OutboundPolicy,
}

impl ParasutConfig {
    /// From an `integrations` row.
    pub fn from_values(config: &Value, secrets: &Value) -> Result<Self, IntegrationError> {
        let c = FieldReader::config(config);
        let s = FieldReader::secrets(secrets);
        let base_url = match c.opt_url("base_url")? {
            Some(u) => u,
            None => Url::parse(LIVE_URL).map_err(|e| IntegrationError::Config(e.to_string()))?,
        };
        let company_id = c.str("company_id")?.trim().to_owned();
        if company_id.is_empty() || !company_id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(IntegrationError::Config(
                "config.company_id must be numeric".into(),
            ));
        }
        Ok(Self {
            base_url,
            company_id,
            client_id: c.str("client_id")?.trim().to_owned(),
            client_secret: s.secret("client_secret")?,
            username: c.str("username")?.trim().to_owned(),
            password: s.secret("password")?,
            commercial: c.bool_or("commercial_scenario", false)?,
            test_company: c.bool_or("test_company", false)?,
            timeout: Duration::from_secs(c.u64_or("timeout_secs", 30)?),
            outbound: OutboundPolicy::global(),
        })
    }
}

#[derive(Debug, Clone)]
struct Token {
    access: SecretString,
    refresh: Option<SecretString>,
    expires_at: DateTime<Utc>,
}

/// Paraşüt client.
pub struct Parasut {
    cfg: ParasutConfig,
    http: HttpClient,
    token: Mutex<Option<Token>>,
}

impl std::fmt::Debug for Parasut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parasut")
            .field("company_id", &self.cfg.company_id)
            .finish_non_exhaustive()
    }
}

fn dec(v: &Value) -> Option<Decimal> {
    match v {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.to_string().parse().ok(),
        _ => None,
    }
}

fn currency_code(c: Currency) -> Option<&'static str> {
    match c.as_str() {
        "TRY" => Some("TRL"),
        "USD" => Some("USD"),
        "EUR" => Some("EUR"),
        "GBP" => Some("GBP"),
        _ => None,
    }
}

fn payment_type(m: PaymentMethod) -> &'static str {
    match m {
        PaymentMethod::Card => "KREDIKARTI/BANKAKARTI",
        PaymentMethod::BankTransfer => "EFT/HAVALE",
        PaymentMethod::Intermediary | PaymentMethod::Other => "ODEMEARACISI",
    }
}

/// `errors[].title/detail` of a JSON:API error document.
fn error_message(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return describe_body(body);
    };
    let parts: Vec<String> = v["errors"]
        .as_array()
        .map(|errs| {
            errs.iter()
                .map(|e| {
                    [e["title"].as_str(), e["detail"].as_str()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(": ")
                })
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if parts.is_empty() {
        v["error_description"]
            .as_str()
            .or(v["error"].as_str())
            .unwrap_or_default()
            .to_owned()
    } else {
        parts.join("; ")
    }
}

impl Parasut {
    /// New client.
    pub fn new(cfg: ParasutConfig) -> Result<Self, IntegrationError> {
        let http = HttpClient::new(
            "parasut",
            cfg.timeout,
            &[&cfg.client_secret, &cfg.password],
            cfg.outbound,
        )?;
        http.check_endpoint(&cfg.base_url)?;
        Ok(Self {
            cfg,
            http,
            token: Mutex::new(None),
        })
    }

    fn url(&self, path: &str) -> Result<Url, IntegrationError> {
        self.cfg
            .base_url
            .join(path)
            .map_err(|e| IntegrationError::Config(format!("parasut: bad URL: {e}")))
    }

    fn api(&self, path: &str) -> Result<Url, IntegrationError> {
        self.url(&format!("v4/{}/{}", self.cfg.company_id, path))
    }

    async fn grant(&self, refresh: Option<&SecretString>) -> Result<Token, IntegrationError> {
        let mut form: Vec<(&str, &str)> = vec![
            ("client_id", &self.cfg.client_id),
            ("client_secret", self.cfg.client_secret.expose_str()),
            ("redirect_uri", "urn:ietf:wg:oauth:2.0:oob"),
        ];
        match refresh {
            Some(r) => {
                form.push(("grant_type", "refresh_token"));
                form.push(("refresh_token", r.expose_str()));
            }
            None => {
                form.push(("grant_type", "password"));
                form.push(("username", &self.cfg.username));
                form.push(("password", self.cfg.password.expose_str()));
            }
        }
        let url = self.url("oauth/token")?;
        let resp = self
            .http
            .execute(
                self.http.client().post(url).form(&form),
                "oauth.token",
                MAX_API_BODY,
            )
            .await?;
        if !resp.ok() {
            let msg = error_message(&resp.body);
            return Err(match resp.status {
                400 | 401 => IntegrationError::Auth(format!("parasut: {}", self.http.scrub(&msg))),
                _ => self.http.status_error(&resp, &msg),
            });
        }
        let v: Value = serde_json::from_slice(&resp.body).map_err(|_| {
            IntegrationError::remote(
                None,
                format!("parasut: token response was {}", describe_body(&resp.body)),
            )
        })?;
        let access = v["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| IntegrationError::Auth("parasut: no access token".into()))?;
        let expires_in = v["expires_in"].as_i64().unwrap_or(7200).clamp(60, 86_400);
        Ok(Token {
            access: SecretString::from(access.to_owned()),
            refresh: v["refresh_token"]
                .as_str()
                .map(|s| SecretString::from(s.to_owned())),
            expires_at: Utc::now() + chrono::Duration::seconds(expires_in - 60),
        })
    }

    async fn access_token(&self, force: bool) -> Result<SecretString, IntegrationError> {
        let mut guard = self.token.lock().await;
        if let Some(t) = guard.as_ref()
            && !force
            && t.expires_at > Utc::now()
        {
            return Ok(t.access.clone());
        }
        let refreshed = match guard.as_ref().and_then(|t| t.refresh.clone()) {
            Some(r) => self.grant(Some(&r)).await.ok(),
            None => None,
        };
        let token = match refreshed {
            Some(t) => t,
            None => self.grant(None).await?,
        };
        let access = token.access.clone();
        *guard = Some(token);
        Ok(access)
    }

    /// Authorised request with one retry on 401 (expired token).
    async fn call(
        &self,
        method: reqwest::Method,
        url: Url,
        body: Option<&Value>,
        operation: &'static str,
        limit: usize,
    ) -> Result<RawResponse, IntegrationError> {
        for attempt in 0..2 {
            let token = self.access_token(attempt > 0).await?;
            let mut req = self
                .http
                .client()
                .request(method.clone(), url.clone())
                .bearer_auth(token.expose_str())
                .header(reqwest::header::ACCEPT, "application/json");
            if let Some(b) = body {
                req = req
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(b.to_string());
            }
            let resp = self.http.execute(req, operation, limit).await?;
            if resp.status == 401 && attempt == 0 {
                continue;
            }
            return Ok(resp);
        }
        unreachable!("loop returns on the second attempt")
    }

    async fn json(
        &self,
        method: reqwest::Method,
        url: Url,
        body: Option<&Value>,
        operation: &'static str,
    ) -> Result<Value, IntegrationError> {
        let resp = self
            .call(method, url, body, operation, MAX_API_BODY)
            .await?;
        if !resp.ok() {
            return Err(self.http.status_error(&resp, &error_message(&resp.body)));
        }
        if resp.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&resp.body).map_err(|_| {
            IntegrationError::remote(
                Some(resp.status),
                format!(
                    "parasut: {operation} returned {}",
                    describe_body(&resp.body)
                ),
            )
        })
    }

    async fn find_or_create_contact(
        &self,
        req: &EDocumentRequest,
    ) -> Result<String, IntegrationError> {
        let c = &req.customer;
        let mut url = self.api("contacts")?;
        let filter = if c.id.is_placeholder() {
            c.email.as_deref().map(|e| ("filter[email]", e))
        } else {
            Some(("filter[tax_number]", c.id.value()))
        };
        if let Some((k, v)) = filter {
            url.query_pairs_mut()
                .append_pair(k, v)
                .append_pair("page[size]", "1");
            let found = self
                .json(reqwest::Method::GET, url, None, "contacts.list")
                .await?;
            if let Some(id) = found["data"][0]["id"].as_str() {
                return Ok(id.to_owned());
            }
        }
        let a = &c.address;
        let city = a
            .state
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&a.city);
        let address = [Some(a.line1.as_str()), a.line2.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let body = json!({"data": {"type": "contacts", "attributes": {
            "name": c.name,
            "contact_type": if matches!(c.id, PartyId::Vkn(_)) { "company" } else { "person" },
            "tax_office": c.tax_office,
            "tax_number": c.id.value(),
            "email": c.email,
            "phone": c.phone,
            "address": address,
            "city": city,
            "district": a.city,
            "country": c.country_name,
            "is_abroad": a.country_code != "TR",
            "account_type": "customer",
        }}});
        let created = self
            .json(
                reqwest::Method::POST,
                self.api("contacts")?,
                Some(&body),
                "contacts.create",
            )
            .await?;
        created["data"]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| IntegrationError::remote(None, "parasut: contact create returned no id"))
    }

    fn description(req: &EDocumentRequest) -> String {
        req.order_reference
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| req.ettn.to_string())
    }

    async fn find_sales_invoice(
        &self,
        req: &EDocumentRequest,
        contact: &str,
    ) -> Result<Option<String>, IntegrationError> {
        let mut url = self.api("sales_invoices")?;
        url.query_pairs_mut()
            .append_pair("filter[contact_id]", contact)
            .append_pair("filter[issue_date]", &req.issue_date.to_string())
            .append_pair("page[size]", "25");
        let found = self
            .json(reqwest::Method::GET, url, None, "sales_invoices.list")
            .await?;
        let wanted = Self::description(req);
        Ok(found["data"].as_array().and_then(|list| {
            list.iter()
                .find(|d| {
                    d["attributes"]["description"].as_str() == Some(wanted.as_str())
                        && d["attributes"]["item_type"].as_str() != Some("cancelled")
                })
                .and_then(|d| d["id"].as_str().map(str::to_owned))
        }))
    }

    async fn create_sales_invoice(
        &self,
        req: &EDocumentRequest,
        contact: &str,
    ) -> Result<String, IntegrationError> {
        let currency = currency_code(req.currency).ok_or(IntegrationError::Unsupported(
            "Paraşüt accepts TRY, USD, EUR and GBP only",
        ))?;
        let c = &req.customer;
        let a = &c.address;
        let details: Vec<Value> = req
            .lines
            .iter()
            .map(|l| {
                json!({"type": "sales_invoice_details", "attributes": {
                    "quantity": l.quantity.normalize().to_string(),
                    "unit_price": l.unit_price.normalize().to_string(),
                    "vat_rate": l.vat_rate.normalize().to_string(),
                    "discount_type": "amount",
                    "discount_value": l.allowance.normalize().to_string(),
                    "description": l.name,
                }})
            })
            .collect();
        let billing_address = [Some(a.line1.as_str()), a.line2.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let mut attrs = json!({
            "item_type": "invoice",
            "description": Self::description(req),
            "issue_date": req.issue_date.to_string(),
            "due_date": req.issue_date.to_string(),
            "currency": currency,
            "billing_address": billing_address,
            "billing_phone": c.phone,
            "tax_office": c.tax_office,
            "tax_number": c.id.value(),
            "city": a.state.as_deref().filter(|s| !s.trim().is_empty()).unwrap_or(&a.city),
            "district": a.city,
            "country": c.country_name,
            "is_abroad": a.country_code != "TR",
        });
        if let Some(x) = req.exchange_rate.filter(|_| req.currency != Currency::TRY) {
            attrs["exchange_rate"] = json!(x.normalize().to_string());
        }
        if let Some(order) = req
            .order_reference
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            attrs["order_no"] = json!(order);
            attrs["order_date"] = json!(req.issue_date.to_string());
        }
        let body = json!({"data": {"type": "sales_invoices", "attributes": attrs, "relationships": {
            "details": {"data": details},
            "contact": {"data": {"id": contact, "type": "contacts"}},
        }}});
        let created = self
            .json(
                reqwest::Method::POST,
                self.api("sales_invoices")?,
                Some(&body),
                "sales_invoices.create",
            )
            .await?;
        let id = created["data"]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                IntegrationError::remote(None, "parasut: sales invoice create returned no id")
            })?;
        let theirs = dec(&created["data"]["attributes"]["net_total"]);
        if theirs != Some(req.totals.payable) {
            // Never issue a legal document whose total differs from ours.
            let _ = self
                .json(
                    reqwest::Method::DELETE,
                    self.api(&format!("sales_invoices/{id}"))?,
                    None,
                    "sales_invoices.delete",
                )
                .await;
            return Err(IntegrationError::InvalidInput(format!(
                "parasut: Paraşüt computed a total of {} but the invoice total is {}; the draft was deleted and no e-document was issued",
                theirs.map_or_else(|| "?".to_owned(), |d| d.normalize().to_string()),
                req.totals.payable.normalize()
            )));
        }
        Ok(id)
    }

    async fn active_e_document(
        &self,
        sales_invoice: &str,
    ) -> Result<Option<(String, String)>, IntegrationError> {
        let mut url = self.api(&format!("sales_invoices/{sales_invoice}"))?;
        url.query_pairs_mut()
            .append_pair("include", "active_e_document");
        let v = self
            .json(reqwest::Method::GET, url, None, "sales_invoices.show")
            .await?;
        let d = &v["data"]["relationships"]["active_e_document"]["data"];
        Ok(match (d["type"].as_str(), d["id"].as_str()) {
            (Some(t @ ("e_archives" | "e_invoices")), Some(id)) => {
                Some((t.to_owned(), id.to_owned()))
            }
            _ => None,
        })
    }

    async fn create_e_document(
        &self,
        req: &EDocumentRequest,
        sales_invoice: &str,
    ) -> Result<String, IntegrationError> {
        let exemption = req.lines.iter().find_map(|l| l.exemption.as_ref());
        let note = req.notes.join("\n");
        let (path, body) = match req.kind {
            DocumentKind::EArsiv => {
                let mut attrs = json!({ "note": note });
                if let Some(ex) = exemption {
                    attrs["vat_exemption_reason_code"] = json!(ex.code);
                    attrs["vat_exemption_reason"] = json!(ex.reason);
                }
                if let Some(s) = &req.internet_sale {
                    attrs["internet_sale"] = json!({
                        "url": s.website,
                        "payment_type": payment_type(s.payment_method),
                        "payment_platform": s.payment_platform,
                        "payment_date": s.payment_date.to_string(),
                    });
                }
                (
                    "e_archives",
                    json!({"data": {"type": "e_archives", "attributes": attrs, "relationships": {
                        "sales_invoice": {"data": {"id": sales_invoice, "type": "sales_invoices"}}
                    }}}),
                )
            }
            DocumentKind::EFatura => {
                let scenario = if req.profile == Profile::TicariFatura || self.cfg.commercial {
                    "commercial"
                } else {
                    "basic"
                };
                let mut attrs =
                    json!({ "scenario": scenario, "to": req.recipient_alias, "note": note });
                if let Some(ex) = exemption {
                    attrs["vat_exemption_reason_code"] = json!(ex.code);
                    attrs["vat_exemption_reason"] = json!(ex.reason);
                }
                (
                    "e_invoices",
                    json!({"data": {"type": "e_invoices", "attributes": attrs, "relationships": {
                        "invoice": {"data": {"id": sales_invoice, "type": "sales_invoices"}}
                    }}}),
                )
            }
        };
        let job = self
            .json(
                reqwest::Method::POST,
                self.api(path)?,
                Some(&body),
                "e_document.create",
            )
            .await?;
        job["data"]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                IntegrationError::remote(None, "parasut: e-document create returned no job id")
            })
    }

    /// Reads an e-document (`e_archives/ID` or `e_invoices/ID`).
    async fn read_e_document(
        &self,
        kind: &str,
        id: &str,
    ) -> Result<DocumentState, IntegrationError> {
        let v = self
            .json(
                reqwest::Method::GET,
                self.api(&format!("{kind}/{id}"))?,
                None,
                "e_document.show",
            )
            .await?;
        let a = &v["data"]["attributes"];
        let status = if kind == "e_archives" {
            DocumentStatus::Issued
        } else {
            match (a["status"].as_str(), a["response_type"].as_str()) {
                (_, Some("rejected")) => DocumentStatus::Rejected,
                (Some("failed"), _) => DocumentStatus::Failed,
                (Some("successful"), _) => DocumentStatus::Issued,
                _ => DocumentStatus::Processing,
            }
        };
        let mut state = DocumentState::new(status);
        state.provider_id = Some(format!("{kind}/{id}"));
        state.number = a["invoice_number"]
            .as_str()
            .or(a["external_id"].as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        state.ettn = a["uuid"].as_str().and_then(|u| Uuid::parse_str(u).ok());
        state.cancellable_until = a["cancellable_until"]
            .as_str()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        if status == DocumentStatus::Rejected {
            state.message = a["response_note"].as_str().map(|s| self.http.scrub(s));
        }
        Ok(state)
    }

    /// Continues from `checkpoint` (job → e-document).
    async fn advance(&self, checkpoint: &mut Value) -> Result<DocumentState, IntegrationError> {
        if let (Some(kind), Some(id)) = (
            checkpoint["e_document_type"].as_str(),
            checkpoint["e_document_id"].as_str(),
        ) {
            let (kind, id) = (kind.to_owned(), id.to_owned());
            return self.read_e_document(&kind, &id).await;
        }
        let sales_invoice = checkpoint["sales_invoice_id"].as_str().map(str::to_owned);
        if let Some(job) = checkpoint["job_id"].as_str().map(str::to_owned) {
            let resp = self
                .call(
                    reqwest::Method::GET,
                    self.api(&format!("trackable_jobs/{job}"))?,
                    None,
                    "trackable_jobs.show",
                    MAX_API_BODY,
                )
                .await?;
            let status = if resp.status == 404 {
                // Jobs expire; fall through to the sales invoice below.
                None
            } else if resp.ok() {
                let v: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
                Some((
                    v["data"]["attributes"]["status"]
                        .as_str()
                        .unwrap_or("running")
                        .to_owned(),
                    v["data"]["attributes"]["errors"].clone(),
                ))
            } else {
                return Err(self.http.status_error(&resp, &error_message(&resp.body)));
            };
            match status.as_ref().map(|(s, _)| s.as_str()) {
                Some("running") => return Ok(DocumentState::new(DocumentStatus::Processing)),
                Some("error") => {
                    let errs = status.map(|(_, e)| e).unwrap_or_default();
                    let msg = errs
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .unwrap_or_default();
                    // Nothing legal exists: a later retry may post again.
                    checkpoint["job_id"] = Value::Null;
                    let mut st = DocumentState::new(DocumentStatus::Failed);
                    st.message = Some(self.http.scrub(if msg.is_empty() {
                        "e-document job failed"
                    } else {
                        &msg
                    }));
                    return Ok(st);
                }
                _ => {}
            }
        }
        let Some(si) = sales_invoice else {
            return Err(IntegrationError::NotFound(
                "parasut: document was never submitted".into(),
            ));
        };
        match self.active_e_document(&si).await? {
            Some((kind, id)) => {
                checkpoint["e_document_type"] = json!(kind);
                checkpoint["e_document_id"] = json!(id);
                self.read_e_document(&kind, &id).await
            }
            None if checkpoint["job_id"].is_string() => {
                // Job finished or expired without an e-document.
                checkpoint["job_id"] = Value::Null;
                let mut st = DocumentState::new(DocumentStatus::Failed);
                st.message = Some("parasut: the e-document job ended without a document".into());
                Ok(st)
            }
            None => Ok(DocumentState::new(DocumentStatus::Processing)),
        }
    }

    fn checkpoint_of(document: &EDocumentRef) -> Value {
        let mut cp = document.checkpoint.clone();
        if !cp.is_object() {
            cp = json!({});
        }
        if let Some((kind, id)) = document
            .provider_id
            .as_deref()
            .and_then(|p| p.split_once('/'))
        {
            cp["e_document_type"] = json!(kind);
            cp["e_document_id"] = json!(id);
        }
        cp
    }
}

#[async_trait]
impl EInvoiceProvider for Parasut {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            cancel_earsiv: false,
            return_invoices: false,
            ubl_download: false,
            assigns_numbers: true,
            foreign_currency: true,
            test_environment: self.cfg.test_company,
        }
    }

    async fn test_connection(&self) -> Result<ConnectionInfo, IntegrationError> {
        let v = self
            .json(reqwest::Method::GET, self.url("v4/me")?, None, "me")
            .await?;
        Ok(ConnectionInfo {
            server_version: Some("API v4".into()),
            details: v["data"]["attributes"]["name"]
                .as_str()
                .map(|n| format!("user {n}")),
        })
    }

    async fn lookup_taxpayer(&self, tax_id: &str) -> Result<TaxpayerInfo, IntegrationError> {
        let mut url = self.api("e_invoice_inboxes")?;
        url.query_pairs_mut().append_pair("filter[vkn]", tax_id);
        let v = self
            .json(reqwest::Method::GET, url, None, "e_invoice_inboxes.list")
            .await?;
        let list = v["data"].as_array().cloned().unwrap_or_default();
        let aliases: Vec<String> = list
            .iter()
            .filter_map(|d| d["attributes"]["e_invoice_address"].as_str())
            .map(str::to_owned)
            .collect();
        Ok(TaxpayerInfo {
            registered: !aliases.is_empty(),
            title: list
                .first()
                .and_then(|d| d["attributes"]["name"].as_str())
                .map(str::to_owned),
            aliases,
        })
    }

    async fn submit(
        &self,
        request: &EDocumentRequest,
        checkpoint: &mut Value,
    ) -> Result<DocumentState, IntegrationError> {
        if request.type_code == InvoiceTypeCode::Iade {
            return Err(IntegrationError::Unsupported(
                "return invoices through Paraşüt",
            ));
        }
        if request.lines.iter().any(|l| l.withholding.is_some()) {
            return Err(IntegrationError::Unsupported("withholding through Paraşüt"));
        }
        // Offline checks with a stand-in number (Paraşüt numbers the document).
        let mut probe = request.clone();
        probe.number = Some(format!("XXX{}000000001", request.issue_date.year()));
        let today: NaiveDate = (Utc::now() + chrono::Duration::hours(3)).date_naive();
        ubl::check(&probe, today).map_err(|e| IntegrationError::InvalidInput(e.to_string()))?;
        if !checkpoint.is_object() {
            *checkpoint = json!({});
        }
        if checkpoint["e_document_id"].is_string() || checkpoint["job_id"].is_string() {
            return self.advance(checkpoint).await;
        }
        let contact = match checkpoint["contact_id"].as_str() {
            Some(c) => c.to_owned(),
            None => {
                let c = self.find_or_create_contact(request).await?;
                checkpoint["contact_id"] = json!(c);
                c
            }
        };
        let sales_invoice = match checkpoint["sales_invoice_id"].as_str() {
            Some(s) => s.to_owned(),
            None => {
                let s = match self.find_sales_invoice(request, &contact).await? {
                    Some(s) => s,
                    None => self.create_sales_invoice(request, &contact).await?,
                };
                checkpoint["sales_invoice_id"] = json!(s);
                s
            }
        };
        if let Some((kind, id)) = self.active_e_document(&sales_invoice).await? {
            checkpoint["e_document_type"] = json!(kind);
            checkpoint["e_document_id"] = json!(id);
            return self.read_e_document(&kind, &id).await;
        }
        let job = self.create_e_document(request, &sales_invoice).await?;
        checkpoint["job_id"] = json!(job);
        self.advance(checkpoint).await
    }

    async fn status(&self, document: &EDocumentRef) -> Result<DocumentState, IntegrationError> {
        let mut cp = Self::checkpoint_of(document);
        self.advance(&mut cp).await
    }

    async fn pdf(&self, document: &EDocumentRef) -> Result<Vec<u8>, IntegrationError> {
        let cp = Self::checkpoint_of(document);
        let (Some(kind), Some(id)) = (cp["e_document_type"].as_str(), cp["e_document_id"].as_str())
        else {
            return Err(IntegrationError::NotFound(
                "parasut: the e-document is not ready".into(),
            ));
        };
        let v = self
            .json(
                reqwest::Method::GET,
                self.api(&format!("{kind}/{id}/pdf"))?,
                None,
                "e_document.pdf",
            )
            .await?;
        let Some(link) = v["data"]["attributes"]["url"].as_str() else {
            return Err(IntegrationError::remote(
                Some(503),
                "parasut: the PDF is not ready yet",
            ));
        };
        let link = Url::parse(link)
            .map_err(|_| IntegrationError::remote(None, "parasut: invalid PDF link"))?;
        // HTTPS only (the configured API origin is accepted as is).
        if link.scheme() != "https" && link.origin() != self.cfg.base_url.origin() {
            return Err(IntegrationError::remote(
                None,
                "parasut: PDF link is not HTTPS",
            ));
        }
        let resp = self
            .http
            .execute(
                self.http.client().get(link),
                "e_document.pdf.download",
                MAX_DOCUMENT_BODY,
            )
            .await?;
        if !resp.ok() || !resp.body.starts_with(b"%PDF") {
            return Err(IntegrationError::remote(
                Some(resp.status),
                format!(
                    "parasut: PDF download returned {}",
                    describe_body(&resp.body)
                ),
            ));
        }
        Ok(resp.body)
    }
}
