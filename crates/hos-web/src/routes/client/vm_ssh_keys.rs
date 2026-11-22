//! `/client/account/ssh-keys` — public SSH keys kept in the client's profile
//! (offered at reinstall and for rescue mode). Keys are validated and
//! fingerprinted in hos-billing (`parse_ssh_public_key`): OpenSSH format,
//! known types, RSA ≥ 2048 bits; private keys are refused. Member area:
//! `services` (the keys grant root on the account's servers).

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use hos_db::repo::vm_ops::SshKeyRow;
use serde::Deserialize;
use uuid::Uuid;

use super::{MemberAccess, audit_client, friendly_error, setup};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentClient, UserAgent};
use crate::flash::{self, SetFlash};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, render};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/account/ssh-keys", get(list).post(add))
        .route("/client/account/ssh-keys/{id}/delete", post(delete))
}

#[derive(Template)]
#[template(path = "client/ssh_keys.html")]
struct KeysPage {
    ctx: PageCtx,
    keys: Vec<SshKeyRow>,
}
crate::page!(KeysPage);

async fn list(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let keys = state
        .billing
        .client_ssh_keys(tenant.id, client.client_id)
        .await?;
    let title = ctx.t("client.ssh_keys.title");
    let crumbs = vec![
        Crumb::link(ctx.t("client.menu.account"), ctx.url("/client/account")),
        Crumb::current(title.clone()),
    ];
    setup(&mut ctx, &client, &access, title, crumbs);
    render(&KeysPage { ctx, keys })
}

#[derive(Debug, Deserialize)]
struct AddForm {
    #[serde(default)]
    name: String,
    #[serde(default)]
    public_key: String,
}

async fn add(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    Form(f): Form<AddForm>,
) -> AppResult<Response> {
    let to = ctx.url("/client/account/ssh-keys");
    check_rate(
        &state.limits.vm_action,
        &format!("sshkey:{}", client.user_id),
    )?;
    match state
        .billing
        .add_client_ssh_key(
            tenant.id,
            client.client_id,
            &f.name,
            &f.public_key,
            Some(client.user_id),
        )
        .await
    {
        Ok(k) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "client.ssh_key.add",
                Some(("ssh_key", k.id)),
            )
            .await;
            Ok(flash::redirect(
                &to,
                SetFlash::success(ctx.tv("client.ssh_keys.added", "fingerprint", &k.fingerprint)),
            ))
        }
        Err(e) => {
            let m = friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?;
            Ok(flash::redirect(&to, SetFlash::error(m)))
        }
    }
}

async fn delete(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<Uuid>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let to = ctx.url("/client/account/ssh-keys");
    match state
        .billing
        .delete_client_ssh_key(tenant.id, client.client_id, id)
        .await
    {
        Ok(()) => {
            audit_client(
                &state,
                &tenant,
                &client,
                ip,
                &ua,
                "client.ssh_key.delete",
                Some(("ssh_key", id)),
            )
            .await;
            Ok(flash::redirect(
                &to,
                SetFlash::success(ctx.t("client.ssh_keys.deleted")),
            ))
        }
        Err(e) => {
            let m = friendly_error(&ctx, &e).ok_or_else(|| AppError::from(e))?;
            Ok(flash::redirect(&to, SetFlash::error(m)))
        }
    }
}
