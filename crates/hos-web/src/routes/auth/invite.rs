//! Staff invitation acceptance (`GET/POST /auth/invite?token=`).
//!
//! The link carries a purpose-bound one-time token (`TokenPurpose::Invite`,
//! only its hash is stored in `staff_invites`). Accepting creates the staff
//! user with the chosen password, assigns the invited role and marks the
//! invitation used — all in one transaction, guarded by the plan's staff
//! limit — then signs the new member in. Unknown, used, revoked and expired
//! tokens all get the same generic message.

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::response::Response;
use hos_auth::tokens::{TokenPurpose, hash_one_time_token};
use hos_core::domain::UserKind;
use hos_db::enums::ActorKind;
use hos_db::repo::admin_settings::{self as srepo, StaffInviteRow};
use hos_db::repo::audit::{self, AuditEntry};
use hos_db::repo::identity::{self, NewUser};
use http::StatusCode;
use serde::Deserialize;
use serde_json::json;

use super::{LoginOutcome, begin_session, hash_password, password_problem};
use crate::error::AppResult;
use crate::extract::{ClientIp, UserAgent};
use crate::forms::{self, FormErrors};
use crate::routes::admin::settings::has_room;
use crate::routes::platform::limits::Limit;
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{PageCtx, render_status};

#[derive(Template)]
#[template(path = "auth/invite.html")]
struct InvitePage {
    ctx: PageCtx,
    token: String,
    /// `None` = invalid / used / expired link.
    invite: Option<StaffInviteRow>,
    first_name: String,
    last_name: String,
    errors: FormErrors,
}
crate::page!(InvitePage);

/// `?token=`.
#[derive(Debug, Default, Deserialize)]
pub struct InviteQuery {
    #[serde(default)]
    token: String,
}

/// Acceptance form.
#[derive(Debug, Default, Deserialize)]
pub struct InviteForm {
    #[serde(default)]
    token: String,
    #[serde(default)]
    first_name: String,
    #[serde(default)]
    last_name: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    password_confirm: String,
}

/// The open invitation behind `token`.
async fn open_invite(
    state: &AppState,
    tenant: &Tenant,
    token: &str,
) -> AppResult<Option<StaffInviteRow>> {
    let token = token.trim();
    if token.is_empty() || token.len() > 200 {
        return Ok(None);
    }
    let hash = hash_one_time_token(TokenPurpose::Invite, token);
    Ok(srepo::staff_invite_by_hash(&state.pool, tenant.id, &hash)
        .await?
        .filter(|i| i.is_open(chrono::Utc::now())))
}

/// `GET /auth/invite?token=`.
pub(super) async fn form(
    State(state): State<AppState>,
    tenant: Tenant,
    Query(q): Query<InviteQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("admin_settings.invite.title");
    ctx.set_title(title);
    let invite = open_invite(&state, &tenant, &q.token).await?;
    let (first_name, last_name) = invite
        .as_ref()
        .map(|i| (i.first_name.clone(), i.last_name.clone()))
        .unwrap_or_default();
    let status = if invite.is_some() {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    render_status(
        status,
        &InvitePage {
            ctx,
            token: if invite.is_some() {
                q.token
            } else {
                String::new()
            },
            invite,
            first_name,
            last_name,
            errors: FormErrors::default(),
        },
    )
}

/// `POST /auth/invite`.
pub(super) async fn submit(
    State(state): State<AppState>,
    tenant: Tenant,
    ip: ClientIp,
    ua: UserAgent,
    mut ctx: PageCtx,
    Form(f): Form<InviteForm>,
) -> AppResult<Response> {
    check_rate(&state.limits.password_reset, &ip.key("staff-invite"))?;
    let title = ctx.t("admin_settings.invite.title");
    ctx.set_title(title);
    let first_name = forms::clean(&f.first_name, 80);
    let last_name = forms::clean(&f.last_name, 80);
    let Some(invite) = open_invite(&state, &tenant, &f.token).await? else {
        return render_status(
            StatusCode::NOT_FOUND,
            &InvitePage {
                ctx,
                token: String::new(),
                invite: None,
                first_name,
                last_name,
                errors: FormErrors::default(),
            },
        );
    };
    let mut errors = FormErrors::default();
    if first_name.is_empty() {
        errors.add("first_name", ctx.t("admin_settings.invite.name_required"));
    }
    if let Some(msg) =
        password_problem(&ctx, &f.password, &[&invite.email, &first_name, &last_name]).await?
    {
        errors.add("password", msg);
    } else if f.password != f.password_confirm {
        errors.add("password_confirm", ctx.t("auth.reset.mismatch"));
    }
    if !errors.any()
        && identity::user_by_email(&state.pool, tenant.id, &invite.email)
            .await?
            .is_some()
    {
        errors.add_general(ctx.t("admin_settings.invite.email_taken"));
    }
    if !errors.any() && !has_room(&state, tenant.id, Limit::Staff).await? {
        errors.add_general(ctx.t("admin_settings.invite.limit"));
    }
    if errors.any() {
        return render_status(
            StatusCode::UNPROCESSABLE_ENTITY,
            &InvitePage {
                ctx,
                token: f.token,
                invite: Some(invite),
                first_name,
                last_name,
                errors,
            },
        );
    }
    let hash = hash_password(&f.password).await?;
    let mut tx = state.pool.begin().await?;
    let user = identity::create_user(
        &mut *tx,
        tenant.id,
        &NewUser {
            email: invite.email.clone(),
            password_hash: Some(hash),
            kind: UserKind::Staff,
            first_name,
            last_name,
            phone: None,
            locale: invite.locale,
            // The link proves control of the mailbox.
            email_verified: true,
        },
    )
    .await?;
    identity::assign_role(&mut *tx, tenant.id, user.id, invite.role_id).await?;
    if !srepo::accept_staff_invite(&mut *tx, tenant.id, invite.id, user.id).await? {
        // Raced with a revoke / second acceptance: nothing is kept.
        tx.rollback().await?;
        return render_status(
            StatusCode::NOT_FOUND,
            &InvitePage {
                ctx,
                token: String::new(),
                invite: None,
                first_name: String::new(),
                last_name: String::new(),
                errors: FormErrors::default(),
            },
        );
    }
    audit::write(
        &mut *tx,
        &AuditEntry::new(
            Some(tenant.id),
            ActorKind::User,
            Some(user.id.into()),
            "staff.invite_accept",
        )
        .request(Some(ip.0), ua.0.as_deref())
        .entity("staff_invite", invite.id)
        .changes(json!({"email": invite.email, "invited_by": invite.invited_by})),
    )
    .await?;
    tx.commit().await?;
    let previous = ctx.session.clone();
    begin_session(
        &state,
        &tenant,
        &ctx,
        ip,
        &ua,
        LoginOutcome {
            user: &user,
            remember: false,
            next: None,
            previous: previous.as_deref(),
        },
    )
    .await
}
