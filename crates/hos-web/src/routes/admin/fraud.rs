//! `/admin/fraud` — order screening: the built-in rules' settings and the
//! ban list (e-mail addresses, e-mail domains, IP addresses / networks).
//! Held orders are reviewed on the order page (`fraud` status). Everything
//! needs `fraud.manage` and is audited.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_auth::rbac::Permission;
use hos_billing::fraud::FraudSettings;
use hos_db::repo::fraud::{self as frepo, BanId, BanKind, BanRow};
use hos_db::repo::platform;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{PermissionMarker, RequirePermission};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, render};

/// `fraud.manage`.
#[derive(Debug, Clone, Copy)]
pub struct FraudManage;
impl PermissionMarker for FraudManage {
    const PERMISSION: Permission = Permission::FRAUD_MANAGE;
}

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/fraud", get(page).post(save_settings))
        .route("/admin/fraud/bans", post(add_ban))
        .route("/admin/fraud/bans/{id}/delete", post(delete_ban))
}

#[derive(Template)]
#[template(path = "admin/fraud.html")]
struct FraudPage {
    ctx: PageCtx,
    s: FraudSettings,
    bans: Vec<BanRow>,
    kinds: Vec<(String, String)>,
}
crate::page!(FraudPage);

async fn page(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<FraudManage>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let s = FraudSettings::load(&state.pool, tenant.id).await?;
    let bans = frepo::list_bans(&state.pool, tenant.id).await?;
    let title = ctx.t("fraud.admin.title");
    common::page(&mut ctx, &staff, &title, &[]);
    render(&FraudPage {
        kinds: common::enum_options(&ctx, BanKind::ALL, |k| k.as_str(), "fraud.kind"),
        s,
        bans,
        ctx,
    })
}

async fn save_settings(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<FraudManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let int = |k: &str, lo: i64, hi: i64| f.int(k).ok().flatten().filter(|v| (lo..=hi).contains(v));
    let (Some(hold), Some(per_ip), Some(clients_ip), Some(per_client)) = (
        int("hold_score", 1, 1000),
        int("max_orders_per_ip", 1, 1000),
        int("max_clients_per_ip", 0, 1000),
        int("max_orders_per_client", 1, 1000),
    ) else {
        return Ok(failed(&ctx, "/admin/fraud", ctx.t("fraud.admin.invalid")));
    };
    let s = FraudSettings {
        enabled: f.flag("enabled"),
        hold_score: hold,
        block_disposable: f.flag("block_disposable"),
        max_orders_per_ip: per_ip,
        max_clients_per_ip: clients_ip,
        max_orders_per_client: per_client,
    };
    for (k, v) in s.to_pairs() {
        platform::set_setting(&state.pool, tenant.id, k, &v).await?;
    }
    au.log(
        &state,
        "settings.fraud",
        None,
        json!(
            s.to_pairs()
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
        ),
    )
    .await?;
    Ok(done(&ctx, "/admin/fraud", "fraud.admin.saved"))
}

/// Normalises a ban value; `None` when it is not a valid value of `kind`.
pub fn normalize_ban(kind: BanKind, raw: &str) -> Option<String> {
    let v = raw.trim().to_lowercase();
    match kind {
        BanKind::Email => {
            let (local, domain) = v.split_once('@')?;
            (!local.is_empty()
                && valid_domain(domain)
                && v.len() <= 254
                && !v.contains(char::is_whitespace))
            .then_some(v)
        }
        BanKind::Domain => {
            let d = v.trim_start_matches('@').trim_end_matches('.').to_owned();
            valid_domain(&d).then_some(d)
        }
        BanKind::Ip => {
            let (addr, prefix) = match v.split_once('/') {
                Some((a, p)) => (a, Some(p)),
                None => (v.as_str(), None),
            };
            let ip: std::net::IpAddr = addr.parse().ok()?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            match prefix {
                None => Some(ip.to_string()),
                Some(p) => {
                    let p: u8 = p.parse().ok()?;
                    (p <= max).then(|| format!("{ip}/{p}"))
                }
            }
        }
    }
}

fn valid_domain(d: &str) -> bool {
    d.len() <= 253
        && d.contains('.')
        && d.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

async fn add_ban(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<FraudManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = "/admin/fraud";
    let Some(kind) = f.parse::<BanKind>("kind") else {
        return Ok(failed(&ctx, back, ctx.t("fraud.admin.invalid")));
    };
    let Some(value) = normalize_ban(kind, &f.text("value", 300)) else {
        return Ok(failed(&ctx, back, ctx.t("fraud.admin.invalid_value")));
    };
    let reason = f.opt("reason", 500);
    match frepo::add_ban(
        &state.pool,
        tenant.id,
        kind,
        &value,
        reason.as_deref(),
        Some(staff.user_id),
    )
    .await
    {
        Ok(row) => {
            au.log(
                &state,
                "fraud.ban_add",
                Some(("fraud_ban", row.id.into_uuid())),
                json!({"kind": kind, "value": row.value}),
            )
            .await?;
            Ok(done(&ctx, back, "fraud.admin.ban_added"))
        }
        Err(e) => Ok(failed(&ctx, back, common::failure_text(&ctx, &e)?)),
    }
}

async fn delete_ban(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<FraudManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = BanId::from(id);
    if !frepo::delete_ban(&state.pool, tenant.id, id).await? {
        return Err(AppError::NotFound);
    }
    au.log(
        &state,
        "fraud.ban_delete",
        Some(("fraud_ban", id.into_uuid())),
        json!({}),
    )
    .await?;
    Ok(done(&ctx, "/admin/fraud", "fraud.admin.ban_deleted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ban_values() {
        assert_eq!(
            normalize_ban(BanKind::Email, " A@Example.COM "),
            Some("a@example.com".into())
        );
        assert_eq!(normalize_ban(BanKind::Email, "nope"), None);
        assert_eq!(
            normalize_ban(BanKind::Domain, "@Spam.Example."),
            Some("spam.example".into())
        );
        assert_eq!(normalize_ban(BanKind::Domain, "bad_domain"), None);
        assert_eq!(normalize_ban(BanKind::Domain, "%.example"), None);
        assert_eq!(
            normalize_ban(BanKind::Ip, "203.0.113.7"),
            Some("203.0.113.7".into())
        );
        assert_eq!(
            normalize_ban(BanKind::Ip, "203.0.113.0/24"),
            Some("203.0.113.0/24".into())
        );
        assert_eq!(
            normalize_ban(BanKind::Ip, "2001:db8::/32"),
            Some("2001:db8::/32".into())
        );
        assert_eq!(normalize_ban(BanKind::Ip, "203.0.113.0/40"), None);
        assert_eq!(normalize_ban(BanKind::Ip, "x"), None);
    }
}
