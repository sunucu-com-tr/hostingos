//! Network status page: `status_components`, `status_incidents`,
//! `status_incident_components`, `status_incident_updates`.

use chrono::{DateTime, Utc};
use hos_core::ids::{ClientId, IntegrationId, TenantId, UserId};
use hos_core::string_enum;
use sqlx::{FromRow, PgConnection, PgExecutor};

use crate::error::{DbResult, conflict_on_unique, not_found};

hos_core::define_id! {
    /// `status_components`.
    pub struct StatusComponentId;
}
hos_core::define_id! {
    /// `status_incidents`.
    pub struct IncidentId;
}
hos_core::define_id! {
    /// `status_incident_updates`.
    pub struct IncidentUpdateId;
}

string_enum! {
    /// `status_components.status`, worst last.
    pub enum ComponentStatus {
        Operational => "operational",
        Maintenance => "maintenance",
        Degraded => "degraded",
        PartialOutage => "partial_outage",
        MajorOutage => "major_outage",
    }
}

impl ComponentStatus {
    /// Severity rank (0 = operational).
    pub fn rank(self) -> u8 {
        match self {
            Self::Operational => 0,
            Self::Maintenance => 1,
            Self::Degraded => 2,
            Self::PartialOutage => 3,
            Self::MajorOutage => 4,
        }
    }
}

string_enum! {
    /// `status_incidents.kind`.
    pub enum IncidentKind {
        Incident => "incident",
        Maintenance => "maintenance",
    }
}

string_enum! {
    /// `status_incidents.impact`.
    pub enum IncidentImpact {
        None => "none",
        Minor => "minor",
        Major => "major",
        Critical => "critical",
    }
}

string_enum! {
    /// `status_incidents.status` / `status_incident_updates.status`.
    pub enum IncidentStatus {
        Investigating => "investigating",
        Identified => "identified",
        Monitoring => "monitoring",
        Resolved => "resolved",
        Scheduled => "scheduled",
        InProgress => "in_progress",
        Completed => "completed",
    }
}

impl IncidentStatus {
    /// Statuses of incidents.
    pub const INCIDENT: &'static [IncidentStatus] = &[
        Self::Investigating,
        Self::Identified,
        Self::Monitoring,
        Self::Resolved,
    ];
    /// Statuses of maintenance windows.
    pub const MAINTENANCE: &'static [IncidentStatus] =
        &[Self::Scheduled, Self::InProgress, Self::Completed];

    /// The incident is over.
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Resolved | Self::Completed)
    }

    /// Valid for `kind`.
    pub fn fits(self, kind: IncidentKind) -> bool {
        match kind {
            IncidentKind::Incident => Self::INCIDENT.contains(&self),
            IncidentKind::Maintenance => Self::MAINTENANCE.contains(&self),
        }
    }
}

/// `status_components` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ComponentRow {
    pub id: StatusComponentId,
    pub tenant_id: TenantId,
    pub name: String,
    pub group_name: String,
    pub description: Option<String>,
    pub status: ComponentStatus,
    pub integration_id: Option<IntegrationId>,
    pub is_public: bool,
    pub sort_order: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `status_incidents` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct IncidentRow {
    pub id: IncidentId,
    pub tenant_id: TenantId,
    pub kind: IncidentKind,
    pub title: String,
    pub impact: IncidentImpact,
    pub status: IncidentStatus,
    pub scheduled_start: Option<DateTime<Utc>>,
    pub scheduled_end: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub is_public: bool,
    pub notified_at: Option<DateTime<Utc>>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// `status_incident_updates` row.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct IncidentUpdateRow {
    pub id: IncidentUpdateId,
    pub tenant_id: TenantId,
    pub incident_id: IncidentId,
    pub status: IncidentStatus,
    pub body: String,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
}

/// Input of [`save_component`].
#[derive(Debug, Clone, PartialEq)]
pub struct ComponentInput {
    pub name: String,
    pub group_name: String,
    pub description: Option<String>,
    pub status: ComponentStatus,
    pub integration_id: Option<IntegrationId>,
    pub is_public: bool,
    pub sort_order: i32,
}

// ---------------------------------------------------------------------------
// components
// ---------------------------------------------------------------------------

/// Creates (`id = None`) or updates a component.
pub async fn save_component(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: Option<StatusComponentId>,
    c: &ComponentInput,
) -> DbResult<ComponentRow> {
    let q = match id {
        None => sqlx::query_as::<_, ComponentRow>(
            "insert into status_components (id, tenant_id, name, group_name, description, status,
                    integration_id, is_public, sort_order)
             values ($2, $1, $3, $4, $5, $6, $7, $8, $9) returning *",
        )
        .bind(tenant_id)
        .bind(StatusComponentId::new()),
        Some(id) => sqlx::query_as::<_, ComponentRow>(
            "update status_components set name = $3, group_name = $4, description = $5, status = $6,
                    integration_id = $7, is_public = $8, sort_order = $9
              where tenant_id = $1 and id = $2 returning *",
        )
        .bind(tenant_id)
        .bind(id),
    };
    q.bind(&c.name)
        .bind(&c.group_name)
        .bind(&c.description)
        .bind(c.status)
        .bind(c.integration_id)
        .bind(c.is_public)
        .bind(c.sort_order)
        .fetch_optional(db)
        .await
        .map_err(conflict_on_unique("component"))?
        .ok_or_else(|| {
            not_found(
                "status component",
                id.map(|i| i.to_string()).unwrap_or_default(),
            )
        })
}

/// Sets only the status of a component.
pub async fn set_component_status(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: StatusComponentId,
    status: ComponentStatus,
) -> DbResult<bool> {
    let res =
        sqlx::query("update status_components set status = $3 where tenant_id = $1 and id = $2")
            .bind(tenant_id)
            .bind(id)
            .bind(status)
            .execute(db)
            .await?;
    Ok(res.rows_affected() == 1)
}

/// Deletes a component.
pub async fn delete_component(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: StatusComponentId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from status_components where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// One component.
pub async fn get_component(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: StatusComponentId,
) -> DbResult<Option<ComponentRow>> {
    Ok(sqlx::query_as::<_, ComponentRow>(
        "select * from status_components where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Components in display order (`public_only`: the status page's).
pub async fn list_components(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    public_only: bool,
) -> DbResult<Vec<ComponentRow>> {
    Ok(sqlx::query_as::<_, ComponentRow>(
        "select * from status_components where tenant_id = $1 and (is_public or not $2)
          order by group_name, sort_order, name",
    )
    .bind(tenant_id)
    .bind(public_only)
    .fetch_all(db)
    .await?)
}

// ---------------------------------------------------------------------------
// incidents
// ---------------------------------------------------------------------------

/// Input of [`create_incident`] / [`update_incident`].
#[derive(Debug, Clone, PartialEq)]
pub struct IncidentInput {
    pub kind: IncidentKind,
    pub title: String,
    pub impact: IncidentImpact,
    pub status: IncidentStatus,
    pub scheduled_start: Option<DateTime<Utc>>,
    pub scheduled_end: Option<DateTime<Utc>>,
    pub is_public: bool,
    pub components: Vec<StatusComponentId>,
}

async fn set_incident_components(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: IncidentId,
    components: &[StatusComponentId],
) -> DbResult<()> {
    sqlx::query("delete from status_incident_components where tenant_id = $1 and incident_id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(&mut *conn)
        .await?;
    if components.is_empty() {
        return Ok(());
    }
    // Only this tenant's components (the composite FK enforces it too).
    sqlx::query(
        "insert into status_incident_components (tenant_id, incident_id, component_id)
         select $1, $2, c.id from status_components c where c.tenant_id = $1 and c.id = any($3)
         on conflict do nothing",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(components)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Creates an incident with its first timeline entry.
pub async fn create_incident(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    i: &IncidentInput,
    first_update: &str,
    created_by: Option<UserId>,
    now: DateTime<Utc>,
) -> DbResult<IncidentRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let row = sqlx::query_as::<_, IncidentRow>(
        "insert into status_incidents (id, tenant_id, kind, title, impact, status, scheduled_start,
                scheduled_end, started_at, resolved_at, is_public, created_by)
         values ($1, $2, $3, $4, $5, $6, $7, $8, coalesce($7, $9), case when $10 then $9 end, $11, $12)
         returning *",
    )
    .bind(IncidentId::new())
    .bind(tenant_id)
    .bind(i.kind)
    .bind(&i.title)
    .bind(i.impact)
    .bind(i.status)
    .bind(i.scheduled_start)
    .bind(i.scheduled_end)
    .bind(now)
    .bind(i.status.is_closed())
    .bind(i.is_public)
    .bind(created_by)
    .fetch_one(&mut *tx)
    .await
    .map_err(conflict_on_unique("incident"))?;
    set_incident_components(&mut tx, tenant_id, row.id, &i.components).await?;
    add_update_in(
        &mut tx,
        tenant_id,
        row.id,
        i.status,
        first_update,
        created_by,
    )
    .await?;
    tx.commit().await?;
    Ok(row)
}

/// Updates the header of an incident (not its timeline).
pub async fn update_incident(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: IncidentId,
    i: &IncidentInput,
) -> DbResult<IncidentRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let row = sqlx::query_as::<_, IncidentRow>(
        "update status_incidents set kind = $3, title = $4, impact = $5, scheduled_start = $6,
                scheduled_end = $7, is_public = $8
          where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(i.kind)
    .bind(&i.title)
    .bind(i.impact)
    .bind(i.scheduled_start)
    .bind(i.scheduled_end)
    .bind(i.is_public)
    .fetch_optional(&mut *tx)
    .await
    .map_err(conflict_on_unique("incident"))?
    .ok_or_else(|| not_found("incident", id))?;
    set_incident_components(&mut tx, tenant_id, id, &i.components).await?;
    tx.commit().await?;
    Ok(row)
}

async fn add_update_in(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: IncidentId,
    status: IncidentStatus,
    body: &str,
    created_by: Option<UserId>,
) -> DbResult<IncidentUpdateRow> {
    sqlx::query_as::<_, IncidentUpdateRow>(
        "insert into status_incident_updates (id, tenant_id, incident_id, status, body, created_by)
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(IncidentUpdateId::new())
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .bind(body)
    .bind(created_by)
    .fetch_one(&mut *conn)
    .await
    .map_err(conflict_on_unique("incident update"))
}

/// Adds a timeline entry and moves the incident to `status`.
pub async fn add_update(
    conn: &mut PgConnection,
    tenant_id: TenantId,
    id: IncidentId,
    status: IncidentStatus,
    body: &str,
    created_by: Option<UserId>,
    now: DateTime<Utc>,
) -> DbResult<IncidentRow> {
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    let row = sqlx::query_as::<_, IncidentRow>(
        "update status_incidents set status = $3,
                resolved_at = case when $4 then coalesce(resolved_at, $5) else null end,
                started_at = case when $3 = 'in_progress' and kind = 'maintenance' then $5 else started_at end
          where tenant_id = $1 and id = $2 returning *",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(status)
    .bind(status.is_closed())
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| not_found("incident", id))?;
    add_update_in(&mut tx, tenant_id, id, status, body, created_by).await?;
    tx.commit().await?;
    Ok(row)
}

/// Marks the incident as notified (once): `true` when this call set it.
pub async fn mark_notified(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: IncidentId,
    now: DateTime<Utc>,
) -> DbResult<bool> {
    let res = sqlx::query(
        "update status_incidents set notified_at = $3
          where tenant_id = $1 and id = $2 and notified_at is null",
    )
    .bind(tenant_id)
    .bind(id)
    .bind(now)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Deletes an incident.
pub async fn delete_incident(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: IncidentId,
) -> DbResult<bool> {
    let res = sqlx::query("delete from status_incidents where tenant_id = $1 and id = $2")
        .bind(tenant_id)
        .bind(id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// One incident.
pub async fn get_incident(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: IncidentId,
) -> DbResult<Option<IncidentRow>> {
    Ok(sqlx::query_as::<_, IncidentRow>(
        "select * from status_incidents where tenant_id = $1 and id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// Incidents, newest first. `public_only` hides private ones; `open_only`
/// keeps those not resolved / completed.
pub async fn list_incidents(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    public_only: bool,
    open_only: bool,
    limit: i64,
) -> DbResult<Vec<IncidentRow>> {
    Ok(sqlx::query_as::<_, IncidentRow>(
        "select * from status_incidents
          where tenant_id = $1 and (is_public or not $2)
            and (not $3 or status not in ('resolved', 'completed'))
          order by coalesce(scheduled_start, started_at) desc, created_at desc limit $4",
    )
    .bind(tenant_id)
    .bind(public_only)
    .bind(open_only)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Public incidents that started (or were scheduled to start) since `since`.
pub async fn incidents_since(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    since: DateTime<Utc>,
    limit: i64,
) -> DbResult<Vec<IncidentRow>> {
    Ok(sqlx::query_as::<_, IncidentRow>(
        "select * from status_incidents
          where tenant_id = $1 and is_public and coalesce(scheduled_start, started_at) >= $2
          order by coalesce(scheduled_start, started_at) desc limit $3",
    )
    .bind(tenant_id)
    .bind(since)
    .bind(limit)
    .fetch_all(db)
    .await?)
}

/// Timeline of an incident, newest first.
pub async fn incident_updates(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: IncidentId,
) -> DbResult<Vec<IncidentUpdateRow>> {
    Ok(sqlx::query_as::<_, IncidentUpdateRow>(
        "select * from status_incident_updates where tenant_id = $1 and incident_id = $2
          order by created_at desc",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(db)
    .await?)
}

/// Component ids of an incident.
pub async fn incident_components(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: IncidentId,
) -> DbResult<Vec<StatusComponentId>> {
    Ok(sqlx::query_scalar::<_, StatusComponentId>(
        "select component_id from status_incident_components where tenant_id = $1 and incident_id = $2",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(db)
    .await?)
}

/// `(incident, component)` pairs of the given incidents.
pub async fn components_of_incidents(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    ids: &[IncidentId],
) -> DbResult<Vec<(IncidentId, StatusComponentId)>> {
    Ok(sqlx::query_as::<_, (IncidentId, StatusComponentId)>(
        "select incident_id, component_id from status_incident_components
          where tenant_id = $1 and incident_id = any($2)",
    )
    .bind(tenant_id)
    .bind(ids)
    .fetch_all(db)
    .await?)
}

/// Open (or upcoming) public incidents that affect a service of `client_id`:
/// a component of the incident is bound to the integration the service runs
/// on. Returns `(incident, affected service labels)`.
pub async fn incidents_for_client(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    client_id: ClientId,
) -> DbResult<Vec<(IncidentRow, Vec<String>)>> {
    #[derive(FromRow)]
    struct Row {
        #[sqlx(flatten)]
        incident: IncidentRow,
        services: Vec<String>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "select i.*,
                array_agg(distinct coalesce(nullif(s.domain, ''), p.name)) as services
           from status_incidents i
           join status_incident_components ic on ic.tenant_id = i.tenant_id and ic.incident_id = i.id
           join status_components c on c.tenant_id = ic.tenant_id and c.id = ic.component_id
           join services s on s.tenant_id = c.tenant_id and s.integration_id = c.integration_id
           join products p on p.tenant_id = s.tenant_id and p.id = s.product_id
          where i.tenant_id = $1 and i.is_public and s.client_id = $2
            and s.status in ('active', 'suspended', 'provisioning')
            and i.status not in ('resolved', 'completed')
          group by i.id
          order by coalesce(i.scheduled_start, i.started_at)",
    )
    .bind(tenant_id)
    .bind(client_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(|r| (r.incident, r.services)).collect())
}

/// Clients with a live service on an integration bound to a component of the
/// incident, with the labels of those services (recipients of the
/// affected-service e-mail).
pub async fn affected_clients(
    db: impl PgExecutor<'_>,
    tenant_id: TenantId,
    id: IncidentId,
) -> DbResult<Vec<(ClientId, Vec<String>)>> {
    Ok(sqlx::query_as::<_, (ClientId, Vec<String>)>(
        "select s.client_id, array_agg(distinct coalesce(nullif(s.domain, ''), p.name))
           from status_incident_components ic
           join status_components c on c.tenant_id = ic.tenant_id and c.id = ic.component_id
           join services s on s.tenant_id = c.tenant_id and s.integration_id = c.integration_id
           join products p on p.tenant_id = s.tenant_id and p.id = s.product_id
           join clients cl on cl.tenant_id = s.tenant_id and cl.id = s.client_id
          where ic.tenant_id = $1 and ic.incident_id = $2
            and s.status in ('active', 'suspended', 'provisioning') and cl.status = 'active'
          group by s.client_id",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_all(db)
    .await?)
}
