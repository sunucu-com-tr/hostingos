//! `/admin/orders` — order list + detail (accept / cancel / mark as fraud)
//! and the manual provisioning queue (`/admin/orders/provisioning`).

use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_billing::provisioning::ManualProvisioning;
use hos_core::domain::OrderStatus;
use hos_core::ids::{OrderId, ServiceId};
use hos_core::secret::SecretString;
use hos_db::repo::admin as arepo;
use hos_db::repo::billing_support;
use hos_db::repo::clients::{self, ClientRow};
use hos_db::repo::domains::DomainRow;
use hos_db::repo::invoices::{self, InvoiceRow};
use hos_db::repo::orders::{self, OrderFilter, OrderItemRow, OrderListRow, OrderRow};
use hos_db::repo::services::{ServiceListRow, ServiceRow};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::common::{self, Audit, FormData, Ui as _, done, failed};
use crate::error::{AppError, AppResult};
use crate::extract::{PageQuery, RequirePermission, perm};
use crate::state::AppState;
use crate::tenant::Tenant;
use crate::view::{PageCtx, Pager, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/orders", get(list))
        .route("/admin/orders/provisioning", get(queue))
        .route(
            "/admin/orders/provisioning/{service}",
            post(complete_manual),
        )
        .route("/admin/orders/bulk", post(bulk))
        .route("/admin/orders/{id}", get(show))
        .route("/admin/orders/{id}/accept", post(accept))
        .route("/admin/orders/{id}/cancel", post(cancel))
        .route("/admin/orders/{id}/fraud", post(fraud))
}

#[derive(Debug, Default, Deserialize)]
struct ListFilter {
    status: Option<String>,
}

#[derive(Template)]
#[template(path = "admin/orders/list.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<OrderListRow>,
    pager: Pager,
    q: String,
    status: String,
    statuses: Vec<(String, String)>,
    /// This list with its filters (where a bulk action returns to).
    back: String,
}
crate::page!(ListPage);

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingView>,
    q: PageQuery,
    Query(f): Query<ListFilter>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let status: Option<OrderStatus> = f.status.as_deref().and_then(|s| s.parse().ok());
    let paged = orders::list_orders(
        &state.pool,
        tenant.id,
        &OrderFilter {
            status,
            client_id: None,
            search: q.q.clone(),
        },
        q.db_page(),
    )
    .await?;
    let title = ctx.t("admin.orders.title");
    common::page(&mut ctx, &staff, &title, &[]);
    let qv = q.q.clone().unwrap_or_default();
    let sv = f.status.clone().unwrap_or_default();
    let pager = Pager::new(
        &paged,
        &ctx.url("/admin/orders"),
        &[("q", qv.as_str()), ("status", sv.as_str())],
    );
    let statuses = common::enum_options(&ctx, OrderStatus::ALL, |s| s.as_str(), "admin.status");
    let back = format!(
        "/admin/orders?{}",
        common::query_string(&[
            ("q", qv.clone()),
            ("status", sv.clone()),
            ("page", q.page.to_string()),
        ])
    );
    render(&ListPage {
        rows: paged.items,
        pager,
        q: qv,
        status: sv,
        statuses,
        back,
        ctx,
    })
}

#[derive(Template)]
#[template(path = "admin/orders/show.html")]
struct ShowPage {
    ctx: PageCtx,
    order: OrderRow,
    client: ClientRow,
    items: Vec<OrderItemRow>,
    invoice: Option<InvoiceRow>,
    services: Vec<ServiceRow>,
    domains: Vec<DomainRow>,
    fraud_json: String,
}
crate::page!(ShowPage);

async fn load(state: &AppState, tenant: &Tenant, id: OrderId) -> AppResult<OrderRow> {
    orders::get_order(&state.pool, tenant.id, id)
        .await?
        .ok_or(AppError::NotFound)
}

async fn show(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingView>,
    Path(id): Path<Uuid>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let order = load(&state, &tenant, id.into()).await?;
    let client = clients::get_client(&state.pool, tenant.id, order.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let items = orders::order_items(&state.pool, tenant.id, order.id).await?;
    let invoice = match order.invoice_id {
        Some(i) => invoices::get_invoice(&state.pool, tenant.id, i).await?,
        None => None,
    };
    let services = billing_support::order_services(&state.pool, tenant.id, order.id).await?;
    let domains = billing_support::order_domains(&state.pool, tenant.id, order.id).await?;
    let title = ctx.tv("admin.orders.show_title", "number", &order.order_number);
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.orders.title", "/admin/orders")],
    );
    let fraud_json = if order
        .fraud_details
        .as_object()
        .is_some_and(|o| !o.is_empty())
    {
        serde_json::to_string_pretty(&order.fraud_details).unwrap_or_default()
    } else {
        String::new()
    };
    render(&ShowPage {
        ctx,
        order,
        client,
        items,
        invoice,
        services,
        domains,
        fraud_json,
    })
}

async fn accept(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let order = load(&state, &tenant, id.into()).await?;
    let back = format!("/admin/orders/{}", order.id);
    // Plan limit (`Services`): the order's pending services are already
    // counted, so acceptance is refused only when the tenant is over the plan.
    let services = crate::routes::platform::limits::plan_limit_status(
        &state,
        tenant.id,
        crate::routes::platform::limits::Limit::Services,
    )
    .await?;
    if services.max.is_some_and(|max| services.used > max) {
        return Ok(common::failed(
            &ctx,
            &back,
            ctx.t("admin_settings.limits.services"),
        ));
    }
    let result = state.billing.accept_order(tenant.id, order.id).await;
    if result.is_ok() {
        au.log(
            &state,
            "order.accept",
            Some(("order", order.id.into())),
            json!({"number": order.order_number}),
        )
        .await?;
    }
    common::outcome(&ctx, &back, result, "admin.orders.accepted")
}

async fn cancel(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let order = load(&state, &tenant, id.into()).await?;
    let back = format!("/admin/orders/{}", order.id);
    let result = state.billing.cancel_order(tenant.id, order.id).await;
    if result.is_ok() {
        au.log(
            &state,
            "order.cancel",
            Some(("order", order.id.into())),
            json!({"number": order.order_number, "from": order.status.as_str()}),
        )
        .await?;
    }
    common::outcome(&ctx, &back, result, "admin.orders.cancelled")
}

/// `POST /admin/orders/bulk` — accepts or cancels the selected pending (or
/// fraud-held) orders through the same billing calls as the order page.
/// Other orders and ids of other tenants are skipped and counted.
async fn bulk(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let back = Some(f.text("back", 600))
        .filter(|b| b.starts_with("/admin/orders"))
        .unwrap_or_else(|| "/admin/orders".to_owned());
    let ids: Vec<OrderId> = f.ids("ids");
    if ids.is_empty() {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.nothing_selected")));
    }
    let action = f.text("action", 20);
    if !matches!(action.as_str(), "accept" | "cancel") {
        return Ok(failed(&ctx, &back, ctx.t("admin.common.invalid_value")));
    }
    let (mut changed, mut skipped, mut over_plan) = (0usize, 0usize, 0usize);
    for id in &ids {
        // As on the order page, checked for every order (the count moves
        // while the selection is worked through): refused when the tenant
        // is over its plan (pending services are counted already).
        if action == "accept" {
            let services = crate::routes::platform::limits::plan_limit_status(
                &state,
                tenant.id,
                crate::routes::platform::limits::Limit::Services,
            )
            .await?;
            if services.max.is_some_and(|max| services.used > max) {
                over_plan += 1;
                continue;
            }
        }
        // Only orders still waiting for a decision (pending, or held as
        // fraud); accepted / completed ones are left to the order page.
        let Some(order) = orders::get_order(&state.pool, tenant.id, *id)
            .await?
            .filter(|o| matches!(o.status, OrderStatus::Pending | OrderStatus::Fraud))
        else {
            skipped += 1;
            continue;
        };
        let result = if action == "accept" {
            state.billing.accept_order(tenant.id, order.id).await
        } else {
            state.billing.cancel_order(tenant.id, order.id).await
        };
        match result {
            Ok(_) => {
                au.log(
                    &state,
                    if action == "accept" {
                        "order.accept"
                    } else {
                        "order.cancel"
                    },
                    Some(("order", order.id.into())),
                    json!({"number": order.order_number, "from": order.status.as_str(), "bulk": true}),
                )
                .await?;
                changed += 1;
            }
            Err(e) if e.http_status() < 500 => skipped += 1,
            Err(e) => return Err(e.into()),
        }
    }
    if over_plan > 0 && changed == 0 {
        return Ok(failed(&ctx, &back, ctx.t("admin_settings.limits.services")));
    }
    let mut text = ctx.tv2(
        "admin.common.bulk_done",
        "done",
        changed,
        "skipped",
        skipped + over_plan,
    );
    if over_plan > 0 {
        text = format!("{text} {}", ctx.t("admin_settings.limits.services"));
    }
    Ok(crate::flash::redirect(
        &ctx.url(&back),
        crate::flash::SetFlash::success(text),
    ))
}

async fn fraud(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::BillingManage>,
    au: Audit,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let order = load(&state, &tenant, id.into()).await?;
    let back = format!("/admin/orders/{}", order.id);
    if order.status != OrderStatus::Pending {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin.orders.fraud_only_pending"),
        ));
    }
    if !arepo::mark_order_fraud(&state.pool, tenant.id, order.id).await? {
        return Ok(failed(
            &ctx,
            &back,
            ctx.t("admin.orders.fraud_only_pending"),
        ));
    }
    let mut details = order.fraud_details.clone();
    if let Some(o) = details.as_object_mut() {
        o.insert("marked_by".into(), json!(staff.user_id));
        o.insert("marked_at".into(), json!(chrono::Utc::now()));
    }
    orders::set_fraud_result(
        &state.pool,
        tenant.id,
        order.id,
        order.fraud_score.unwrap_or_default(),
        &details,
    )
    .await?;
    au.log(
        &state,
        "order.fraud",
        Some(("order", order.id.into())),
        json!({"number": order.order_number}),
    )
    .await?;
    Ok(done(&ctx, &back, "admin.orders.marked_fraud"))
}

// ---------------------------------------------------------------------------
// Manual provisioning queue
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/orders/queue.html")]
struct QueuePage {
    ctx: PageCtx,
    rows: Vec<ServiceListRow>,
}
crate::page!(QueuePage);

async fn queue(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesView>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = arepo::manual_queue(&state.pool, tenant.id).await?;
    let title = ctx.t("admin.orders.queue_title");
    common::page(
        &mut ctx,
        &staff,
        &title,
        &[("admin.orders.title", "/admin/orders")],
    );
    render(&QueuePage { ctx, rows })
}

async fn complete_manual(
    State(state): State<AppState>,
    tenant: Tenant,
    RequirePermission { staff, .. }: RequirePermission<perm::ServicesManage>,
    au: Audit,
    Path(service): Path<Uuid>,
    ctx: PageCtx,
    f: FormData,
) -> AppResult<Response> {
    let service_id = ServiceId::from(service);
    let back = "/admin/orders/provisioning";
    let ip = match f.opt("dedicated_ip", 64) {
        Some(v) => match v.parse() {
            Ok(ip) => Some(ip),
            Err(_) => return Ok(failed(&ctx, back, ctx.t("admin.common.invalid_ip"))),
        },
        None => None,
    };
    let details = ManualProvisioning {
        username: f.opt("username", 100),
        password: f.opt("password", 200).map(SecretString::new),
        external_id: f.opt("external_id", 200),
        dedicated_ip: ip,
        server_hostname: f.opt("server_hostname", 255),
        note: f.opt("note", 2000),
        staff_user_id: Some(staff.user_id),
    };
    let result = state
        .billing
        .complete_manual_provisioning(tenant.id, service_id, &details)
        .await;
    if result.is_ok() {
        au.log(
            &state,
            "service.manual_provision",
            Some(("service", service_id.into())),
            json!({"username": details.username, "external_id": details.external_id}),
        )
        .await?;
    }
    common::outcome(&ctx, back, result, "admin.orders.provisioned")
}
