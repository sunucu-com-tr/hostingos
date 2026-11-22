//! Custom domains of the tenant (`/admin/settings/domains`): add (plan limit
//! `CustomDomains`), remove, make primary and "verify now". Verification is
//! the platform's DNS check (`routes::platform::domains`: TXT
//! `_hostingos.<host>`, CNAME to `<slug>.<HOS_BASE_DOMAIN>` or a matching
//! A/AAAA record); unverified domains are re-checked by the maintenance loop.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_core::ids::TenantDomainId;
use hos_db::enums::TenantDomainKind;
use hos_db::repo::admin_settings as srepo;
use hos_db::repo::platform;
use hos_db::repo::saas::{self, DomainCheckRow};
use serde_json::json;

use super::{has_room, invalidate_caches, page_chrome, tabs};
use crate::error::{AppError, AppResult};
use crate::extract::{RequirePermission, perm};
use crate::flash::{self, SetFlash};
use crate::routes::admin::common::{Audit, FormData, done, failed};
use crate::routes::platform::domains as dns;
use crate::routes::platform::limits::Limit;
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{NavItem, PageCtx, render};

const BACK: &str = "/admin/settings/domains";

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/settings/domains", get(list).post(add))
        .route("/admin/settings/domains/{id}/verify", post(verify))
        .route("/admin/settings/domains/{id}/primary", post(primary))
        .route("/admin/settings/domains/{id}/delete", post(remove))
}

/// One row of the table.
#[derive(Debug, Clone)]
pub struct DomainView {
    pub row: DomainCheckRow,
    pub custom: bool,
    pub verified: bool,
    pub txt_name: String,
    pub txt_value: String,
}

#[derive(Template)]
#[template(path = "admin/settings/domains.html")]
struct DomainsPage {
    ctx: PageCtx,
    tabs: Vec<NavItem>,
    active: String,
    rows: Vec<DomainView>,
    cname_target: String,
    used: i64,
    max: Option<i64>,
    can_add: bool,
}
crate::page!(DomainsPage);

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::SettingsManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = saas::tenant_domain_checks(&state.pool, tenant.id)
        .await?
        .into_iter()
        .map(|r| {
            let token = r.domain.verification_token.clone().unwrap_or_default();
            DomainView {
                custom: r.domain.kind == TenantDomainKind::Custom,
                verified: r.domain.verified_at.is_some(),
                txt_name: dns::txt_name(&r.domain.host),
                txt_value: dns::txt_value(&token),
                row: r,
            }
        })
        .collect();
    let status =
        crate::routes::platform::limits::plan_limit_status(&state, tenant.id, Limit::CustomDomains)
            .await?;
    page_chrome(&mut ctx, &staff, "admin_settings.tabs.domains");
    render(&DomainsPage {
        tabs: tabs(&ctx),
        active: ctx.url(BACK),
        rows,
        cname_target: format!(
            "{}.{}",
            tenant.slug,
            state.config.base_domain.trim_matches('.')
        ),
        used: status.used,
        max: status.max,
        can_add: status.allows(1),
        ctx,
    })
}

async fn add(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let host = platform::normalize_host(&f.text("host", 253));
    let base = state
        .config
        .base_domain
        .trim_matches('.')
        .to_ascii_lowercase();
    if !crate::routes::platform::tenants::valid_host(&host)
        || host == base
        || host.ends_with(&format!(".{base}"))
    {
        return Ok(failed(
            &ctx,
            BACK,
            ctx.t("admin_settings.domains.invalid_host"),
        ));
    }
    if !has_room(&state, tenant.id, Limit::CustomDomains).await? {
        return Ok(failed(
            &ctx,
            BACK,
            ctx.t("admin_settings.limits.custom_domains"),
        ));
    }
    let mut conn = state.pool.acquire().await?;
    match platform::add_tenant_domain(
        &mut conn,
        tenant.id,
        &host,
        TenantDomainKind::Custom,
        false,
        false,
    )
    .await
    {
        Ok(row) => {
            au.log(
                &state,
                "settings.domain_add",
                Some(("tenant_domain", row.id.into())),
                json!({"host": host}),
            )
            .await?;
            Ok(done(&ctx, BACK, "admin_settings.domains.added"))
        }
        Err(hos_core::CoreError::Conflict(_)) => {
            Ok(failed(&ctx, BACK, ctx.t("admin_settings.domains.taken")))
        }
        Err(e) => Err(e.into()),
    }
}

async fn load(state: &AppState, tenant: &Tenant, id: &str) -> AppResult<DomainCheckRow> {
    let id: TenantDomainId = id.parse().map_err(|_| AppError::NotFound)?;
    saas::get_tenant_domain(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)
}

async fn verify(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load(&state, &tenant, &id).await?;
    if row.domain.kind != TenantDomainKind::Custom || row.domain.verified_at.is_some() {
        return Ok(done(&ctx, BACK, "admin_settings.domains.already_verified"));
    }
    let ok = dns::verify_domain(&state, &row).await?;
    au.log(
        &state,
        "settings.domain_verify",
        Some(("tenant_domain", row.domain.id.into())),
        json!({"host": row.domain.host, "verified": ok}),
    )
    .await?;
    let flash = if ok {
        SetFlash::success(ctx.t("admin_settings.domains.verified"))
    } else {
        SetFlash::warning(ctx.t("admin_settings.domains.not_verified"))
    };
    Ok(flash::redirect(&ctx.url(BACK), flash))
}

async fn primary(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load(&state, &tenant, &id).await?;
    let mut conn = state.pool.acquire().await?;
    if !srepo::set_primary_domain(&mut conn, tenant.id, row.domain.id).await? {
        return Ok(failed(
            &ctx,
            BACK,
            ctx.t("admin_settings.domains.verify_first"),
        ));
    }
    au.log(
        &state,
        "settings.domain_primary",
        Some(("tenant_domain", row.domain.id.into())),
        json!({"host": row.domain.host}),
    )
    .await?;
    invalidate_caches(&state, &tenant);
    Ok(done(&ctx, BACK, "admin_settings.domains.primary_set"))
}

async fn remove(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::SettingsManage>,
    au: Audit,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let row = load(&state, &tenant, &id).await?;
    if row.domain.kind != TenantDomainKind::Custom || row.domain.is_primary {
        return Ok(failed(
            &ctx,
            BACK,
            ctx.t("admin_settings.domains.cannot_remove"),
        ));
    }
    platform::delete_tenant_domain(&state.pool, tenant.id, row.domain.id).await?;
    au.log(
        &state,
        "settings.domain_remove",
        Some(("tenant_domain", row.domain.id.into())),
        json!({"host": row.domain.host}),
    )
    .await?;
    invalidate_caches(&state, &tenant);
    Ok(done(&ctx, BACK, "admin.common.deleted"))
}

impl DomainsPage {
    fn limit_text(&self) -> String {
        match self.max {
            Some(m) => self.tv2("admin_settings.domains.usage", "used", self.used, "max", m),
            None => self.tv("admin_settings.domains.usage_unlimited", "used", self.used),
        }
    }
}
