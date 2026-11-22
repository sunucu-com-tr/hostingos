//! Shared building blocks of the admin handlers: a permissive form reader
//! ([`FormData`], repeated keys allowed), the audit-log writer ([`Audit`]),
//! template helpers ([`Ui`]), flash/redirect shorthands, CSV export and
//! sortable table headers.

use std::net::IpAddr;
use std::ops::Deref;

use askama::filters::Safe;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use chrono::{DateTime, NaiveDate, Utc};
use hos_core::CoreError;
use hos_core::money::Currency;
use hos_db::enums::ActorKind;
use hos_db::repo::audit::{self, AuditEntry};
use http::request::Parts;
use http::{HeaderValue, header};
use rust_decimal::Decimal;
use serde_json::Value;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentStaff, PageQuery, SortDir, UserAgent};
use crate::flash::{self, SetFlash};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{Crumb, NavItem, PageCtx, PanelChrome, escape_html};

// ---------------------------------------------------------------------------
// Form bodies
// ---------------------------------------------------------------------------

/// An `application/x-www-form-urlencoded` body as ordered pairs. Unlike
/// `axum::Form` it keeps repeated keys (checkbox lists, bulk selections).
#[derive(Debug, Clone, Default)]
pub struct FormData(pub Vec<(String, String)>);

impl<S: Send + Sync> FromRequest<S> for FormData {
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, AppError> {
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|_| AppError::PayloadTooLarge)?;
        Ok(Self::from_bytes(&bytes))
    }
}

impl FormData {
    /// Parses an urlencoded body.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(url::form_urlencoded::parse(bytes).into_owned().collect())
    }

    /// First raw value of `key` (`""` when absent).
    pub fn raw(&self, key: &str) -> &str {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    /// Trimmed value, at most `max` characters.
    pub fn text(&self, key: &str, max: usize) -> String {
        self.raw(key).trim().chars().take(max).collect()
    }

    /// Trimmed, non-empty value.
    pub fn opt(&self, key: &str, max: usize) -> Option<String> {
        let v = self.text(key, max);
        (!v.is_empty()).then_some(v)
    }

    /// Multi-line text: `\r\n` normalised, at most `max` characters.
    pub fn body(&self, key: &str, max: usize) -> String {
        self.raw(key)
            .replace("\r\n", "\n")
            .trim()
            .chars()
            .take(max)
            .collect()
    }

    /// Every value of `key` (non-empty, trimmed).
    pub fn all(&self, key: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(k, v)| k == key && !v.trim().is_empty())
            .map(|(_, v)| v.trim().to_owned())
            .collect()
    }

    /// Whether a key was sent at all.
    pub fn has(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }

    /// Checkbox semantics: present with a truthy value.
    pub fn flag(&self, key: &str) -> bool {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .any(|(_, v)| matches!(v.as_str(), "1" | "on" | "true" | "yes"))
    }

    /// Decimal (`1.234,50` and `1234.50` both accepted). `Ok(None)` when blank.
    #[allow(clippy::result_unit_err)]
    pub fn decimal(&self, key: &str) -> Result<Option<Decimal>, ()> {
        parse_decimal(self.raw(key))
    }

    /// Integer; `Ok(None)` when blank.
    #[allow(clippy::result_unit_err)]
    pub fn int(&self, key: &str) -> Result<Option<i64>, ()> {
        let v = self.raw(key).trim();
        if v.is_empty() {
            return Ok(None);
        }
        v.parse::<i64>().map(Some).map_err(|_| ())
    }

    /// Date `YYYY-MM-DD`; `Ok(None)` when blank.
    #[allow(clippy::result_unit_err)]
    pub fn date(&self, key: &str) -> Result<Option<NaiveDate>, ()> {
        let v = self.raw(key).trim();
        if v.is_empty() {
            return Ok(None);
        }
        NaiveDate::parse_from_str(v, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| ())
    }

    /// `datetime-local` value (`YYYY-MM-DDTHH:MM`) in UTC; `Ok(None)` when blank.
    #[allow(clippy::result_unit_err)]
    pub fn datetime(&self, key: &str, tz: &str) -> Result<Option<DateTime<Utc>>, ()> {
        parse_local_datetime(self.raw(key), tz)
    }

    /// A UUID-backed id (`ClientId`, …); `None` when blank / malformed.
    pub fn id<T: From<Uuid>>(&self, key: &str) -> Option<T> {
        Uuid::parse_str(self.raw(key).trim()).ok().map(T::from)
    }

    /// Every UUID of `key`.
    pub fn ids<T: From<Uuid>>(&self, key: &str) -> Vec<T> {
        self.all(key)
            .iter()
            .filter_map(|v| Uuid::parse_str(v).ok().map(T::from))
            .collect()
    }

    /// A `string_enum` value; `None` when blank / unknown.
    pub fn parse<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.raw(key).trim().parse().ok()
    }
}

/// Parses a decimal typed by a person (`1.234,50`, `1,234.50`, `12,5`, `12.5`).
#[allow(clippy::result_unit_err)]
pub fn parse_decimal(raw: &str) -> Result<Option<Decimal>, ()> {
    let s: String = raw.trim().chars().filter(|c| !c.is_whitespace()).collect();
    if s.is_empty() {
        return Ok(None);
    }
    let normalized = match (s.rfind(','), s.rfind('.')) {
        (Some(c), Some(d)) if c > d => s.replace('.', "").replace(',', "."),
        (Some(_), Some(_)) => s.replace(',', ""),
        (Some(_), None) => s.replace(',', "."),
        _ => s,
    };
    normalized.parse::<Decimal>().map(Some).map_err(|_| ())
}

/// Parses `YYYY-MM-DDTHH:MM` in time zone `tz` into UTC.
#[allow(clippy::result_unit_err)]
pub fn parse_local_datetime(raw: &str, tz: &str) -> Result<Option<DateTime<Utc>>, ()> {
    use chrono::TimeZone as _;
    let v = raw.trim();
    if v.is_empty() {
        return Ok(None);
    }
    let naive = chrono::NaiveDateTime::parse_from_str(v, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S"))
        .map_err(|_| ())?;
    let zone = crate::i18n::parse_tz(tz);
    zone.from_local_datetime(&naive)
        .earliest()
        .map(|d| Some(d.with_timezone(&Utc)))
        .ok_or(())
}

/// `datetime-local` input value of `dt` in `tz`.
pub fn local_input(dt: &Option<DateTime<Utc>>, tz: &str) -> String {
    dt.map(|d| {
        d.with_timezone(&crate::i18n::parse_tz(tz))
            .format("%Y-%m-%dT%H:%M")
            .to_string()
    })
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Multipart (logo / favicon uploads)
// ---------------------------------------------------------------------------

/// One part of a `multipart/form-data` body.
#[derive(Debug, Clone, Default)]
pub struct Part {
    /// Field name.
    pub name: String,
    /// File name (file parts).
    pub filename: Option<String>,
    /// Declared content type.
    pub content_type: Option<String>,
    /// Raw bytes.
    pub data: Vec<u8>,
}

/// A parsed `multipart/form-data` body.
#[derive(Debug, Clone, Default)]
pub struct Multipart(pub Vec<Part>);

impl<S: Send + Sync> FromRequest<S> for Multipart {
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, AppError> {
        let ct = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let boundary = ct
            .split(';')
            .map(str::trim)
            .find_map(|p| p.strip_prefix("boundary="))
            .map(|b| b.trim_matches('"').to_owned())
            .ok_or_else(|| AppError::bad_request("multipart boundary missing"))?;
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|_| AppError::PayloadTooLarge)?;
        Ok(Self(parse_multipart(&bytes, &boundary)))
    }
}

impl Multipart {
    /// Text value of a non-file field.
    pub fn text(&self, name: &str) -> String {
        self.0
            .iter()
            .find(|p| p.name == name && p.filename.is_none())
            .map(|p| String::from_utf8_lossy(&p.data).trim().to_owned())
            .unwrap_or_default()
    }

    /// A non-empty file part.
    pub fn file(&self, name: &str) -> Option<&Part> {
        self.0
            .iter()
            .find(|p| p.name == name && p.filename.is_some() && !p.data.is_empty())
    }

    /// The non-file fields as [`FormData`] (repeated keys kept).
    pub fn form(&self) -> FormData {
        FormData(
            self.0
                .iter()
                .filter(|p| p.filename.is_none())
                .map(|p| {
                    (
                        p.name.clone(),
                        String::from_utf8_lossy(&p.data).into_owned(),
                    )
                })
                .collect(),
        )
    }
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Splits a multipart body on `boundary`.
pub fn parse_multipart(body: &[u8], boundary: &str) -> Vec<Part> {
    let delim = format!("--{boundary}");
    let delim = delim.as_bytes();
    let mut parts = Vec::new();
    let Some(mut pos) = find(body, delim, 0) else {
        return parts;
    };
    loop {
        let start = pos + delim.len();
        if body.get(start..start + 2) == Some(b"--") {
            break;
        }
        let Some(head_end) = find(body, b"\r\n\r\n", start) else {
            break;
        };
        let Some(next) = find(body, delim, head_end + 4) else {
            break;
        };
        let head = String::from_utf8_lossy(&body[start..head_end]);
        let mut data_end = next;
        if data_end >= 2 && &body[data_end - 2..data_end] == b"\r\n" {
            data_end -= 2;
        }
        let mut part = Part {
            data: body[head_end + 4..data_end.max(head_end + 4)].to_vec(),
            ..Part::default()
        };
        for line in head.lines() {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("content-disposition:") {
                for attr in line.split(';').map(str::trim) {
                    if let Some(v) = attr.strip_prefix("name=") {
                        part.name = v.trim_matches('"').to_owned();
                    } else if let Some(v) = attr.strip_prefix("filename=") {
                        part.filename = Some(v.trim_matches('"').to_owned());
                    }
                }
            } else if lower.starts_with("content-type:") {
                part.content_type = line
                    .split_once(':')
                    .map(|(_, v)| v.trim().to_ascii_lowercase());
            }
        }
        parts.push(part);
        pos = next;
    }
    parts
}

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

/// Request facts for `audit_log` rows (tenant, acting staff user, IP, UA).
#[derive(Debug, Clone)]
pub struct Audit {
    tenant_id: hos_core::ids::TenantId,
    actor: Option<Uuid>,
    ip: IpAddr,
    user_agent: Option<String>,
}

impl FromRequestParts<AppState> for Audit {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let tenant = Tenant::from_request_parts(parts, state).await?;
        let staff = CurrentStaff::from_request_parts(parts, state).await?;
        let ip = ClientIp::for_request(parts, state);
        let UserAgent(user_agent) = UserAgent::from_request_parts(parts, state)
            .await
            .unwrap_or_default();
        Ok(Self {
            tenant_id: tenant.id,
            actor: Some(staff.user_id.into()),
            ip: ip.0,
            user_agent,
        })
    }
}

impl Audit {
    /// Client IP of the request.
    pub fn ip(&self) -> IpAddr {
        self.ip
    }

    /// Writes one `audit_log` row (`entity` = `(type, id)`).
    pub async fn log(
        &self,
        state: &AppState,
        action: &str,
        entity: Option<(&str, Uuid)>,
        changes: Value,
    ) -> AppResult<()> {
        let mut entry = AuditEntry::new(Some(self.tenant_id), ActorKind::User, self.actor, action)
            .request(Some(self.ip), self.user_agent.as_deref())
            .changes(changes);
        if let Some((t, id)) = entity {
            entry = entry.entity(t, id);
        }
        audit::write(&state.pool, &entry).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// Redirect to `path` (localized) with a translated success flash.
pub fn done(ctx: &PageCtx, path: &str, key: &str) -> Response {
    flash::redirect(&ctx.url(path), SetFlash::success(ctx.t(key)))
}

/// Redirect to `path` with an error flash.
pub fn failed(ctx: &PageCtx, path: &str, text: impl Into<String>) -> Response {
    flash::redirect(&ctx.url(path), SetFlash::error(text))
}

/// Maps the result of a business call: success → redirect + flash `ok_key`;
/// a user-facing failure (validation, state conflict, provider error) →
/// redirect + error flash with the reason; anything else propagates.
pub fn outcome<T>(
    ctx: &PageCtx,
    path: &str,
    result: Result<T, CoreError>,
    ok_key: &str,
) -> AppResult<Response> {
    match result {
        Ok(_) => Ok(done(ctx, path, ok_key)),
        Err(e) => Ok(failed(ctx, path, failure_text(ctx, &e)?)),
    }
}

/// Text for a failed action, or the error itself when it is not user-facing.
pub fn failure_text(ctx: &PageCtx, e: &CoreError) -> AppResult<String> {
    let status = e.http_status();
    if status < 500 || (502..=504).contains(&status) {
        Ok(ctx.tv("admin.common.failed", "reason", e))
    } else {
        Err(AppError::internal(e))
    }
}

/// `POST /admin/preview/markdown` (htmx): renders the first submitted field
/// as safe Markdown (editor previews).
pub async fn markdown_preview(_staff: CurrentStaff, f: FormData) -> Response {
    let body =
        f.0.iter()
            .find(|(k, _)| k != "csrf_token")
            .map(|(_, v)| v.chars().take(100_000).collect::<String>())
            .unwrap_or_default();
    fragment(crate::markdown::render(&body))
}

/// A small HTML fragment response (htmx).
pub fn fragment(html: String) -> Response {
    axum::response::Html(html).into_response()
}

// ---------------------------------------------------------------------------
// Panel chrome
// ---------------------------------------------------------------------------

/// Admin chrome with breadcrumbs `(label, href)`; the last crumb is current.
pub fn chrome(ctx: &PageCtx, staff: &CurrentStaff, trail: &[(String, String)]) -> PanelChrome {
    let n = trail.len();
    let crumbs = trail
        .iter()
        .enumerate()
        .map(|(i, (label, href))| {
            if i + 1 == n || href.is_empty() {
                Crumb::current(label.clone())
            } else {
                Crumb::link(label.clone(), ctx.url(href))
            }
        })
        .collect();
    super::menu::chrome(ctx, staff, crumbs)
}

/// Sets title + chrome in one call.
pub fn page(ctx: &mut PageCtx, staff: &CurrentStaff, title: &str, trail: &[(&str, &str)]) {
    ctx.set_title(title.to_owned());
    let mut owned: Vec<(String, String)> = trail
        .iter()
        .map(|(k, h)| (ctx.t(k), (*h).to_owned()))
        .collect();
    owned.push((title.to_owned(), String::new()));
    ctx.panel = Some(chrome(ctx, staff, &owned));
}

/// Tab bar items `(label key, path)`.
pub fn tabs(ctx: &PageCtx, items: &[(&str, String)]) -> Vec<NavItem> {
    items
        .iter()
        .map(|(key, path)| NavItem {
            label: ctx.t(key),
            href: ctx.url(path),
            description: String::new(),
            icon: String::new(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

/// A sortable column header.
#[derive(Debug, Clone)]
pub struct SortHeader {
    /// Label.
    pub label: String,
    /// Link toggling the sort.
    pub href: String,
    /// Sorted by this column.
    pub active: bool,
    /// Descending (when active).
    pub desc: bool,
    /// `aria-sort` value.
    pub aria: &'static str,
    /// Right-aligned numeric column.
    pub num: bool,
}

/// Builds sortable headers `(key, label key, numeric)`; `keep` are the other
/// query parameters (filters) to preserve.
pub fn sort_headers(
    ctx: &PageCtx,
    base: &str,
    q: &PageQuery,
    cols: &[(&str, &str, bool)],
    keep: &[(&str, String)],
) -> Vec<SortHeader> {
    cols.iter()
        .map(|(key, label, num)| {
            let active = q.sort.as_deref() == Some(*key);
            let desc = active && q.dir == SortDir::Desc;
            let next_dir = if active && !desc { "desc" } else { "asc" };
            let mut ser = url::form_urlencoded::Serializer::new(String::new());
            for (k, v) in keep.iter().filter(|(_, v)| !v.is_empty()) {
                ser.append_pair(k, v);
            }
            ser.append_pair("sort", key);
            ser.append_pair("dir", next_dir);
            SortHeader {
                label: ctx.t(label),
                href: format!("{}?{}", ctx.url(base), ser.finish()),
                active,
                desc,
                aria: if !active {
                    "none"
                } else if desc {
                    "descending"
                } else {
                    "ascending"
                },
                num: *num,
            }
        })
        .collect()
}

/// `ORDER BY` fragment from an allow-list (`asc`/`desc` appended).
pub fn order_by(q: &PageQuery, allowed: &[(&str, &'static str)], default: &'static str) -> String {
    let col = q.sort_column(allowed, "");
    if col.is_empty() {
        default.to_owned()
    } else {
        format!("{col} {}", q.dir.as_str())
    }
}

/// Query string of the active filters (for pager / export links).
pub fn query_string(pairs: &[(&str, String)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs.iter().filter(|(_, v)| !v.is_empty()) {
        ser.append_pair(k, v);
    }
    ser.finish()
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

/// One CSV cell: quoted when needed; a leading `= + - @`, tab or CR is
/// prefixed with `'` so spreadsheets do not evaluate it (formula injection).
pub fn csv_cell(value: &str) -> String {
    let mut v = value.to_owned();
    if v.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        v.insert(0, '\'');
    }
    if v.contains([',', '"', '\n', '\r', ';']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v
    }
}

/// `text/csv` download (UTF-8 with BOM so spreadsheets detect the encoding).
pub fn csv_response(filename: &str, header_row: &[String], rows: &[Vec<String>]) -> Response {
    let mut out = String::from("\u{feff}");
    let line = |cells: &[String]| {
        cells
            .iter()
            .map(|c| csv_cell(c))
            .collect::<Vec<_>>()
            .join(",")
    };
    out.push_str(&line(header_row));
    out.push_str("\r\n");
    for r in rows {
        out.push_str(&line(r));
        out.push_str("\r\n");
    }
    let mut res = out.into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    let safe: String = filename
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    res
}

// ---------------------------------------------------------------------------
// Template helpers
// ---------------------------------------------------------------------------

/// Extra template helpers for admin pages (every `crate::page!` struct gets
/// them): `{{ sbadge(row.status.as_str()) }}`, `{{ d(row.due_date) }}` …
pub trait Ui: Deref<Target = PageCtx> {
    /// Status badge; label from `admin.status.*`, then `common.status.*`.
    fn sbadge(&self, status: &str) -> Safe<String> {
        let label = self.lbl("admin.status", status);
        self.badge(&label, crate::view::status_tone(status))
    }

    /// Translation of `prefix.value` (falls back to `common.status.value`,
    /// then the value itself).
    fn lbl(&self, prefix: &str, value: &str) -> String {
        let key = format!("{prefix}.{value}");
        if self.cat.has(&key) {
            return self.t(&key);
        }
        let common = format!("common.status.{value}");
        if self.cat.has(&common) {
            return self.t(&common);
        }
        value.replace('_', " ")
    }

    /// Calendar date.
    fn d(&self, date: &NaiveDate) -> String {
        hos_billing::format::date(*date, self.locale)
    }

    /// Optional calendar date (`—`).
    fn dopt(&self, date: &Option<NaiveDate>) -> String {
        date.map(|d| self.d(&d)).unwrap_or_else(|| "—".into())
    }

    /// Optional timestamp (`—`).
    fn dtopt(&self, dt: &Option<DateTime<Utc>>) -> String {
        dt.map(|d| self.datetime(&d)).unwrap_or_else(|| "—".into())
    }

    /// Optional text (`—`).
    fn o(&self, s: &Option<String>) -> String {
        s.as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("—")
            .to_owned()
    }

    /// Money in `currency`.
    fn m(&self, amount: &Decimal, currency: &Currency) -> String {
        self.money_in(amount, currency)
    }

    /// Plain decimal for input values (`12.5`).
    fn dec(&self, d: &Decimal) -> String {
        d.normalize().to_string()
    }

    /// Safe Markdown → HTML.
    fn md(&self, s: &str) -> Safe<String> {
        Safe(crate::markdown::render(s))
    }

    /// Yes / no label.
    fn yesno(&self, b: &bool) -> String {
        self.t(if *b { "common.yes" } else { "common.no" })
    }

    /// ` selected` when equal.
    fn sel(&self, a: &str, b: &str) -> &'static str {
        if a == b { " selected" } else { "" }
    }

    /// ` checked` when true.
    fn chk(&self, b: &bool) -> &'static str {
        if *b { " checked" } else { "" }
    }

    /// Escaped text with newlines as `<br>`.
    fn nl2br(&self, s: &str) -> Safe<String> {
        Safe(escape_html(s).replace('\n', "<br>"))
    }

    /// `datetime-local` input value in the tenant time zone.
    fn dtl(&self, dt: &Option<DateTime<Utc>>) -> String {
        local_input(dt, &self.timezone)
    }

    /// Short id (first 8 hex chars) for compact references.
    fn short(&self, id: &Uuid) -> String {
        id.simple().to_string()[..8].to_owned()
    }
}

impl<T: Deref<Target = PageCtx>> Ui for T {}

/// `(value, label)` pairs of a `string_enum`'s values, labelled through `prefix`.
pub fn enum_options<T: Copy>(
    ctx: &PageCtx,
    all: &[T],
    as_str: impl Fn(&T) -> &'static str,
    prefix: &str,
) -> Vec<(String, String)> {
    all.iter()
        .map(|v| {
            let s = as_str(v);
            let key = format!("{prefix}.{s}");
            let label = if ctx.cat.has(&key) {
                ctx.t(&key)
            } else if ctx.cat.has(&format!("common.status.{s}")) {
                ctx.t(&format!("common.status.{s}"))
            } else {
                s.replace('_', " ")
            };
            (s.to_owned(), label)
        })
        .collect()
}

/// Today in the tenant time zone.
pub fn today(tz: &str) -> NaiveDate {
    Utc::now()
        .with_timezone(&crate::i18n::parse_tz(tz))
        .date_naive()
}

/// Truncates `s` to `max` characters with an ellipsis.
pub fn ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_escapes_formulas() {
        assert_eq!(csv_cell("=SUM(A1)"), "'=SUM(A1)");
        assert_eq!(csv_cell("+90 555"), "'+90 555");
        assert_eq!(csv_cell("-1"), "'-1");
        assert_eq!(csv_cell("@cmd"), "'@cmd");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_cell("=1,2"), "\"'=1,2\"");
        assert_eq!(csv_cell("Ayşe"), "Ayşe");
    }

    #[test]
    fn decimals_parse_both_locales() {
        assert_eq!(parse_decimal("1.234,50"), Ok(Some(Decimal::new(123450, 2))));
        assert_eq!(parse_decimal("1,234.50"), Ok(Some(Decimal::new(123450, 2))));
        assert_eq!(parse_decimal("12,5"), Ok(Some(Decimal::new(125, 1))));
        assert_eq!(parse_decimal(" 99 "), Ok(Some(Decimal::from(99))));
        assert_eq!(parse_decimal(""), Ok(None));
        assert!(parse_decimal("abc").is_err());
    }

    #[test]
    fn form_keeps_repeated_keys() {
        let f = FormData::from_bytes(b"ids=a&ids=b&name=+x+&on=1");
        assert_eq!(f.all("ids"), vec!["a", "b"]);
        assert_eq!(f.text("name", 10), "x");
        assert!(f.flag("on"));
        assert!(!f.flag("off"));
    }

    #[test]
    fn multipart_parts() {
        let body = b"--XB\r\nContent-Disposition: form-data; name=\"csrf_token\"\r\n\r\nabc\r\n--XB\r\nContent-Disposition: form-data; name=\"logo\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n\x89PNG\r\n\r\n--XB--\r\n";
        let parts = parse_multipart(body, "XB");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "csrf_token");
        assert_eq!(parts[0].data, b"abc");
        assert_eq!(parts[1].filename.as_deref(), Some("a.png"));
        assert_eq!(parts[1].content_type.as_deref(), Some("image/png"));
        assert_eq!(parts[1].data, b"\x89PNG\r\n");
    }
}
