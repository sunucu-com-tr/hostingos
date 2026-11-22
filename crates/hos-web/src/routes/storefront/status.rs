//! Public network status page of a tenant:
//!
//! * `GET /status` — overall state, components by group, open incidents,
//!   scheduled / running maintenance, the last 14 days and the editable copy
//!   of the CMS page `status` / `durum` (merged route, see
//!   [`super::landing::RESERVED`]);
//! * `GET /status/incidents/{id}` — one public incident with its timeline;
//! * `GET /status/feed.xml` — Atom feed of public incidents (90 days).
//!
//! Only `is_public` components and incidents are shown; update bodies go
//! through the sanitising Markdown renderer.

use std::collections::BTreeMap;

use askama::Template;
use axum::extract::{Path, State};
use axum::response::Response;
use chrono::{Duration, Utc};
use hos_db::repo::status::{
    self as srepo, ComponentRow, ComponentStatus, IncidentId, IncidentKind, IncidentRow,
    IncidentUpdateRow,
};
use hos_seo::escape::xml as xml_escape;

use crate::error::{AppError, AppResult};
use crate::markdown;
use crate::routes::admin::common::Ui as _;
use crate::seo::{self, SeoPage};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// CMS slugs whose copy is merged into `/status`.
pub const COPY_SLUGS: &[&str] = &["status", "durum"];

/// A component group on the page.
pub struct GroupView {
    pub name: String,
    pub components: Vec<ComponentRow>,
}

/// An incident with its latest update and affected component names.
pub struct IncidentView {
    pub incident: IncidentRow,
    pub latest: Option<IncidentUpdateRow>,
    pub components: Vec<String>,
}

#[derive(Template)]
#[template(path = "storefront/status.html")]
struct StatusPage {
    ctx: PageCtx,
    title: String,
    level: &'static str,
    groups: Vec<GroupView>,
    open: Vec<IncidentView>,
    maintenance: Vec<IncidentView>,
    history: Vec<IncidentView>,
    blocks: Vec<super::blocks::Block>,
}
crate::page!(StatusPage);

#[derive(Template)]
#[template(path = "storefront/status_incident.html")]
struct IncidentPage {
    ctx: PageCtx,
    v: IncidentView,
    updates: Vec<IncidentUpdateRow>,
}
crate::page!(IncidentPage);

/// Overall level of the banner: `ok`, `maintenance`, `minor`, `major`.
pub fn overall_level(components: &[ComponentRow], open: &[IncidentRow]) -> &'static str {
    let worst = components
        .iter()
        .map(|c| c.status.rank())
        .max()
        .unwrap_or(0);
    let incident_major = open.iter().any(|i| {
        i.kind == IncidentKind::Incident && matches!(i.impact.as_str(), "major" | "critical")
    });
    let incident_minor = open.iter().any(|i| i.kind == IncidentKind::Incident);
    let maint_running = open
        .iter()
        .any(|i| i.kind == IncidentKind::Maintenance && i.status.as_str() == "in_progress");
    if worst >= ComponentStatus::PartialOutage.rank() || incident_major {
        "major"
    } else if worst >= ComponentStatus::Degraded.rank() || incident_minor {
        "minor"
    } else if worst == ComponentStatus::Maintenance.rank() || maint_running {
        "maintenance"
    } else {
        "ok"
    }
}

async fn views(
    state: &AppState,
    tenant: &Tenant,
    incidents: Vec<IncidentRow>,
    names: &BTreeMap<String, String>,
) -> AppResult<Vec<IncidentView>> {
    let ids: Vec<IncidentId> = incidents.iter().map(|i| i.id).collect();
    let pairs = srepo::components_of_incidents(&state.pool, tenant.id, &ids).await?;
    let mut out = Vec::with_capacity(incidents.len());
    for incident in incidents {
        let latest = srepo::incident_updates(&state.pool, tenant.id, incident.id)
            .await?
            .into_iter()
            .next();
        let components = pairs
            .iter()
            .filter(|(i, _)| *i == incident.id)
            .filter_map(|(_, c)| names.get(&c.to_string()).cloned())
            .collect();
        out.push(IncidentView {
            incident,
            latest,
            components,
        });
    }
    Ok(out)
}

/// `GET /status`.
pub(super) async fn page(
    State(state): State<AppState>,
    tenant: Tenant,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    ctx.load_store().await?;
    let components = srepo::list_components(&state.pool, tenant.id, true).await?;
    let names: BTreeMap<String, String> = components
        .iter()
        .map(|c| (c.id.to_string(), c.name.clone()))
        .collect();
    let open_rows: Vec<IncidentRow> =
        srepo::list_incidents(&state.pool, tenant.id, true, true, 50).await?;
    let level = overall_level(&components, &open_rows);
    let (maint_rows, incident_rows): (Vec<_>, Vec<_>) = open_rows
        .into_iter()
        .partition(|i| i.kind == IncidentKind::Maintenance);
    let history_rows: Vec<IncidentRow> =
        srepo::incidents_since(&state.pool, tenant.id, Utc::now() - Duration::days(14), 50)
            .await?
            .into_iter()
            .filter(|i| i.status.is_closed())
            .collect();
    let mut groups: Vec<GroupView> = Vec::new();
    for c in components {
        match groups.iter_mut().find(|g| g.name == c.group_name) {
            Some(g) => g.components.push(c),
            None => groups.push(GroupView {
                name: c.group_name.clone(),
                components: vec![c],
            }),
        }
    }
    let mut copy = None;
    for slug in COPY_SLUGS {
        if let Some(c) = super::landing::load(&state, &tenant, &ctx, slug).await? {
            copy = Some(c);
            break;
        }
    }
    let title = copy
        .as_ref()
        .map(|c| c.page.title.clone())
        .unwrap_or_else(|| ctx.t("netstatus.public.title"));
    let blocks = copy
        .as_ref()
        .map(super::landing::Landing::body)
        .unwrap_or_default();
    let page = SeoPage::new(title.clone(), "/status")
        .description(ctx.t("netstatus.public.description"))
        .crumb(title.clone(), "/status");
    seo::apply(&mut ctx, page).await?;
    let open = views(&state, &tenant, incident_rows, &names).await?;
    let maintenance = views(&state, &tenant, maint_rows, &names).await?;
    let history = views(&state, &tenant, history_rows, &names).await?;
    render(&StatusPage {
        title,
        level,
        groups,
        open,
        maintenance,
        history,
        blocks,
        ctx,
    })
}

/// `GET /status/incidents/{id}`.
pub(super) async fn incident(
    State(state): State<AppState>,
    tenant: Tenant,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let id: IncidentId = id.parse().map_err(|_| AppError::NotFound)?;
    let row = srepo::get_incident(&state.pool, tenant.id, id)
        .await?
        .filter(|i| i.is_public)
        .ok_or(AppError::NotFound)?;
    ctx.load_store().await?;
    let components = srepo::list_components(&state.pool, tenant.id, true).await?;
    let names: BTreeMap<String, String> = components
        .iter()
        .map(|c| (c.id.to_string(), c.name.clone()))
        .collect();
    let updates = srepo::incident_updates(&state.pool, tenant.id, id).await?;
    let v = views(&state, &tenant, vec![row], &names)
        .await?
        .pop()
        .ok_or(AppError::NotFound)?;
    let status_title = ctx.t("netstatus.public.title");
    let description = updates
        .first()
        .map(|u| hos_seo::meta::description_from_text(&markdown::plain_text(&u.body), 160))
        .unwrap_or_default();
    let page = SeoPage::new(v.incident.title.clone(), format!("/status/incidents/{id}"))
        .description(description)
        .crumb(status_title, "/status")
        .crumb(v.incident.title.clone(), format!("/status/incidents/{id}"));
    seo::apply(&mut ctx, page).await?;
    render(&IncidentPage { v, updates, ctx })
}

/// `GET /status/feed.xml` (Atom): public incidents and maintenance of the
/// last 90 days, each entry carrying its latest update.
pub(super) async fn feed(
    State(state): State<AppState>,
    tenant: Tenant,
    ctx: PageCtx,
) -> AppResult<Response> {
    let rows = srepo::incidents_since(&state.pool, tenant.id, Utc::now() - Duration::days(90), 100)
        .await?;
    let abs = |p: &str| format!("{}{}", ctx.origin, ctx.url(p));
    let updated = rows
        .iter()
        .map(|i| i.updated_at)
        .max()
        .unwrap_or_else(Utc::now);
    let title = format!("{} — {}", ctx.site_name, ctx.t("netstatus.public.title"));
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    out.push_str(&format!(
        "<feed xmlns=\"http://www.w3.org/2005/Atom\" xml:lang=\"{}\">\n<title>{}</title>\n<id>{}</id>\n<link rel=\"self\" href=\"{}\"/>\n<link rel=\"alternate\" type=\"text/html\" href=\"{}\"/>\n<updated>{}</updated>\n",
        ctx.lang,
        xml_escape(&title),
        xml_escape(&abs("/status")),
        xml_escape(&abs("/status/feed.xml")),
        xml_escape(&abs("/status")),
        updated.to_rfc3339(),
    ));
    for i in &rows {
        let url = abs(&format!("/status/incidents/{}", i.id));
        let latest = srepo::incident_updates(&state.pool, tenant.id, i.id).await?;
        let body = latest.first().map(|u| u.body.clone()).unwrap_or_default();
        let label = ctx.t(&format!("netstatus.incident.{}", i.status.as_str()));
        out.push_str(&format!(
            "<entry>\n<title>{} [{}]</title>\n<id>{}</id>\n<link rel=\"alternate\" type=\"text/html\" href=\"{}\"/>\n<published>{}</published>\n<updated>{}</updated>\n<author><name>{}</name></author>\n<content type=\"html\">{}</content>\n</entry>\n",
            xml_escape(&i.title),
            xml_escape(&label),
            xml_escape(&url),
            xml_escape(&url),
            i.created_at.to_rfc3339(),
            i.updated_at.to_rfc3339(),
            xml_escape(&ctx.site_name),
            xml_escape(&markdown::render(&body)),
        ));
    }
    out.push_str("</feed>\n");
    let mut res = Response::new(axum::body::Body::from(out));
    res.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/atom+xml; charset=utf-8"),
    );
    res.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("public, max-age=300"),
    );
    Ok(res)
}

impl StatusPage {
    fn level_label(&self) -> String {
        self.ctx
            .t(&format!("netstatus.public.level.{}", self.level))
    }
    fn cstate(&self, s: &ComponentStatus) -> String {
        self.lbl("netstatus.component", s.as_str())
    }
    fn istatus(&self, v: &IncidentView) -> String {
        self.lbl("netstatus.incident", v.incident.status.as_str())
    }
}

impl IncidentPage {
    fn istatus(&self, s: &str) -> String {
        self.lbl("netstatus.incident", s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hos_core::ids::TenantId;
    use hos_db::repo::status::{IncidentImpact, IncidentStatus, StatusComponentId};

    fn comp(status: ComponentStatus) -> ComponentRow {
        ComponentRow {
            id: StatusComponentId::new(),
            tenant_id: TenantId::new(),
            name: "x".into(),
            group_name: String::new(),
            description: None,
            status,
            integration_id: None,
            is_public: true,
            sort_order: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn inc(kind: IncidentKind, impact: IncidentImpact, status: IncidentStatus) -> IncidentRow {
        IncidentRow {
            id: IncidentId::new(),
            tenant_id: TenantId::new(),
            kind,
            title: "t".into(),
            impact,
            status,
            scheduled_start: None,
            scheduled_end: None,
            started_at: Utc::now(),
            resolved_at: None,
            is_public: true,
            notified_at: None,
            created_by: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn overall_levels() {
        assert_eq!(
            overall_level(&[comp(ComponentStatus::Operational)], &[]),
            "ok"
        );
        assert_eq!(
            overall_level(&[comp(ComponentStatus::Maintenance)], &[]),
            "maintenance"
        );
        assert_eq!(
            overall_level(&[comp(ComponentStatus::Degraded)], &[]),
            "minor"
        );
        assert_eq!(
            overall_level(&[comp(ComponentStatus::MajorOutage)], &[]),
            "major"
        );
        let scheduled = inc(
            IncidentKind::Maintenance,
            IncidentImpact::Minor,
            IncidentStatus::Scheduled,
        );
        assert_eq!(
            overall_level(&[], &[scheduled]),
            "ok",
            "upcoming maintenance is not an outage"
        );
        let running = inc(
            IncidentKind::Maintenance,
            IncidentImpact::Minor,
            IncidentStatus::InProgress,
        );
        assert_eq!(overall_level(&[], &[running]), "maintenance");
        let critical = inc(
            IncidentKind::Incident,
            IncidentImpact::Critical,
            IncidentStatus::Identified,
        );
        assert_eq!(
            overall_level(&[comp(ComponentStatus::Operational)], &[critical]),
            "major"
        );
    }
}
