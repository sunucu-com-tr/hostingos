//! Account: profile and preferences, security (password, 2FA, linked
//! sign-in methods, sessions), sub-users and contacts, API keys,
//! notification preferences, activity log and privacy (data export,
//! deletion request). Credential changes are refused while a staff member
//! is impersonating the client.

use askama::Template;
use axum::Router;
use axum::extract::{Form, Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::{DateTime, Utc};
use hos_core::domain::{Locale, TicketPriority, UserKind};
use hos_core::ids::{ApiKeyId, ContactId, SessionId, UserId};
use hos_db::Page;
use hos_db::enums::{AuthorKind, MessageSource, OAuthProvider};
use hos_db::repo::audit::{self, AuditFilter, AuditLogRow};
use hos_db::repo::clients::{self, ClientMemberRow, ContactInput, ContactRow};
use hos_db::repo::identity::{self, ApiKeyRow, NewUser, SessionRow, UserIdentityRow};
use hos_db::repo::{domains as domain_repo, invoices, services as service_repo, support};
use http::HeaderValue;
use http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::json;

use super::{
    MEMBER_AREAS, MemberAccess, audit_client, credentials_locked, no_store, parse_id,
    require_own_session, setup,
};
use crate::error::{AppError, AppResult};
use crate::extract::{ClientIp, CurrentClient, UserAgent};
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::routes::auth::oauth::{ProviderButton, buttons};
use crate::routes::auth::{hash_password, password_problem};
use crate::state::{AppState, check_rate};
use crate::tenant::Tenant;
use crate::view::{Crumb, PageCtx, render, render_status};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/client/account", get(profile).post(profile_save))
        .route("/client/account/security", get(security))
        .route("/client/account/password", post(change_password))
        .route("/client/account/identities/{provider}/unlink", post(unlink))
        .route("/client/account/sessions/{id}/revoke", post(revoke_session))
        .route(
            "/client/account/sessions/revoke-others",
            post(revoke_others),
        )
        .route("/client/account/users", get(users).post(invite))
        .route(
            "/client/account/users/{uid}/permissions",
            post(set_permissions),
        )
        .route("/client/account/users/{uid}/remove", post(remove_user))
        .route("/client/account/contacts", get(contacts).post(contact_add))
        .route("/client/account/contacts/{id}/delete", post(contact_delete))
        .route(
            "/client/account/api-keys",
            get(api_keys).post(api_key_create),
        )
        .route("/client/account/api-keys/{id}/revoke", post(api_key_revoke))
        .route(
            "/client/account/notifications",
            get(notifications).post(notifications_save),
        )
        .route("/client/account/activity", get(activity))
        .route("/client/account/privacy", get(privacy))
        .route("/client/account/privacy/export", get(export))
        .route("/client/account/privacy/delete", post(delete_request))
}

/// Account section tabs `(href, label key, icon)`.
const TABS: &[(&str, &str, &str)] = &[
    ("/client/account", "client.account.tab_profile", "user"),
    (
        "/client/account/security",
        "client.account.tab_security",
        "shield",
    ),
    (
        "/client/account/billing",
        "client.account.tab_billing",
        "building",
    ),
    ("/client/account/users", "client.account.tab_users", "users"),
    (
        "/client/account/contacts",
        "client.account.tab_contacts",
        "mail",
    ),
    ("/client/account/api-keys", "client.account.tab_api", "key"),
    (
        "/client/account/notifications",
        "client.account.tab_notifications",
        "bell",
    ),
    (
        "/client/account/activity",
        "client.account.tab_activity",
        "activity",
    ),
    (
        "/client/account/privacy",
        "client.account.tab_privacy",
        "lock",
    ),
];

/// Tabs this user may open.
pub fn tabs(ctx: &PageCtx, access: &MemberAccess) -> Vec<crate::view::NavItem> {
    TABS.iter()
        .filter(|(href, _, _)| super::area_of(href).is_none_or(|a| access.allows(a)))
        .map(|(href, key, icon)| crate::view::NavItem {
            label: ctx.t(key),
            href: ctx.url(href),
            description: String::new(),
            icon: (*icon).to_owned(),
        })
        .collect()
}

fn account_setup(
    ctx: &mut PageCtx,
    client: &CurrentClient,
    access: &MemberAccess,
    key: &str,
) -> (Vec<crate::view::NavItem>, String) {
    let title = ctx.t(key);
    let crumbs = vec![
        Crumb::link(ctx.t("client.account.title"), ctx.url("/client/account")),
        Crumb::current(title.clone()),
    ];
    setup(ctx, client, access, title, crumbs);
    (tabs(ctx, access), ctx.url(&ctx.path))
}

// ---------------------------------------------------------------------------
// Profile & preferences
// ---------------------------------------------------------------------------

/// Common time zones offered (IANA).
pub const TIMEZONES: &[&str] = &[
    "Europe/Istanbul",
    "UTC",
    "Europe/London",
    "Europe/Berlin",
    "Europe/Amsterdam",
    "Europe/Paris",
    "Europe/Moscow",
    "Asia/Baku",
    "Asia/Dubai",
    "Asia/Tbilisi",
    "Asia/Almaty",
    "America/New_York",
    "America/Chicago",
    "America/Los_Angeles",
    "Asia/Tokyo",
    "Australia/Sydney",
];

#[derive(Debug, Default, Clone, Deserialize)]
struct ProfileForm {
    #[serde(default)]
    first_name: String,
    #[serde(default)]
    last_name: String,
    #[serde(default)]
    phone: String,
    #[serde(default)]
    locale: String,
    #[serde(default)]
    timezone: String,
}

#[derive(Template)]
#[template(path = "client/account.html")]
struct ProfilePage {
    ctx: PageCtx,
    tabs: Vec<crate::view::NavItem>,
    active: String,
    form: ProfileForm,
    errors: FormErrors,
    email: String,
    locales: Vec<(String, String)>,
    timezones: Vec<(String, String)>,
}
crate::page!(ProfilePage);

fn profile_page(
    mut ctx: PageCtx,
    client: &CurrentClient,
    access: &MemberAccess,
    form: ProfileForm,
    errors: FormErrors,
) -> ProfilePage {
    let (tabs, active) = account_setup(&mut ctx, client, access, "client.account.title");
    let locales = Locale::ALL
        .iter()
        .map(|l| {
            (
                l.as_str().to_owned(),
                ctx.t(&format!("client.account.locale_{}", l.as_str())),
            )
        })
        .collect();
    let timezones = std::iter::once((String::new(), ctx.t("client.account.timezone_default")))
        .chain(TIMEZONES.iter().map(|t| ((*t).to_owned(), (*t).to_owned())))
        .collect();
    ProfilePage {
        email: client.user.email.clone(),
        ctx,
        tabs,
        active,
        form,
        errors,
        locales,
        timezones,
    }
}

async fn profile(client: CurrentClient, access: MemberAccess, ctx: PageCtx) -> AppResult<Response> {
    let u = &client.user;
    let form = ProfileForm {
        first_name: u.first_name.clone(),
        last_name: u.last_name.clone(),
        phone: u.phone.clone().unwrap_or_default(),
        locale: u.locale.as_str().to_owned(),
        timezone: u.timezone.clone().unwrap_or_default(),
    };
    render(&profile_page(
        ctx,
        &client,
        &access,
        form,
        FormErrors::default(),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn profile_save(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    Form(f): Form<ProfileForm>,
) -> AppResult<Response> {
    let mut errors = FormErrors::default();
    let first = forms::clean(&f.first_name, 100);
    let last = forms::clean(&f.last_name, 100);
    if first.is_empty() {
        errors.add("first_name", ctx.t("client.common.required"));
    }
    if last.is_empty() {
        errors.add("last_name", ctx.t("client.common.required"));
    }
    let locale: Locale = f.locale.parse().unwrap_or(client.user.locale);
    let tz = f.timezone.trim();
    if !tz.is_empty() && tz.parse::<chrono_tz::Tz>().is_err() {
        errors.add("timezone", ctx.t("client.account.timezone_invalid"));
    }
    if errors.any() {
        return render_status(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            &profile_page(ctx, &client, &access, f, errors),
        );
    }
    let phone = forms::clean(&f.phone, 32);
    identity::update_user_profile(
        &state.pool,
        tenant.id,
        client.user_id,
        &first,
        &last,
        (!phone.is_empty()).then_some(phone.as_str()),
        locale,
    )
    .await?;
    identity::set_user_timezone(
        &state.pool,
        tenant.id,
        client.user_id,
        (!tz.is_empty()).then_some(tz),
    )
    .await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.profile.updated",
        None,
    )
    .await;
    let to = ctx.url_in("/client/account", &locale);
    Ok(flash::redirect(
        &to,
        SetFlash::success(ctx.t("client.account.saved")),
    ))
}

// ---------------------------------------------------------------------------
// Security
// ---------------------------------------------------------------------------

/// A session row for display.
pub struct SessionView {
    /// Id.
    pub id: String,
    /// This browser.
    pub current: bool,
    /// Browser summary.
    pub agent: String,
    /// IP.
    pub ip: String,
    /// Last activity.
    pub last_seen: DateTime<Utc>,
    /// Staff impersonation session.
    pub impersonated: bool,
}

#[derive(Template)]
#[template(path = "client/security.html")]
struct SecurityPage {
    ctx: PageCtx,
    tabs: Vec<crate::view::NavItem>,
    active: String,
    locked: bool,
    has_password: bool,
    totp: bool,
    identities: Vec<UserIdentityRow>,
    providers: Vec<ProviderButton>,
    sessions: Vec<SessionView>,
    errors: FormErrors,
}
crate::page!(SecurityPage);

impl SecurityPage {
    fn can_unlink(&self) -> bool {
        self.has_password || self.identities.len() > 1
    }
    fn linked(&self, kind: &str) -> bool {
        self.identities.iter().any(|i| i.provider.as_str() == kind)
    }
}

/// Short browser / OS summary of a user agent.
pub fn agent_summary(ua: Option<&str>) -> String {
    let ua = ua.unwrap_or("");
    let browser = ["Edg/", "OPR/", "Firefox/", "Chrome/", "Safari/"]
        .iter()
        .find(|b| ua.contains(**b))
        .map(|b| match *b {
            "Edg/" => "Edge",
            "OPR/" => "Opera",
            "Firefox/" => "Firefox",
            "Chrome/" => "Chrome",
            _ => "Safari",
        });
    let os = [
        ("Windows", "Windows"),
        ("iPhone", "iOS"),
        ("iPad", "iPadOS"),
        ("Android", "Android"),
        ("Mac OS X", "macOS"),
        ("Linux", "Linux"),
    ]
    .iter()
    .find(|(k, _)| ua.contains(k))
    .map(|(_, v)| *v);
    match (browser, os) {
        (Some(b), Some(o)) => format!("{b} · {o}"),
        (Some(b), None) => b.to_owned(),
        (None, Some(o)) => o.to_owned(),
        (None, None) if ua.is_empty() => "—".to_owned(),
        (None, None) => ua.chars().take(60).collect(),
    }
}

async fn security_page(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    access: &MemberAccess,
    mut ctx: PageCtx,
    errors: FormErrors,
) -> AppResult<SecurityPage> {
    let identities = identity::list_identities(&state.pool, tenant.id, client.user_id).await?;
    let providers = buttons(state, tenant, &ctx, Some("/client/account/security")).await?;
    let sessions = identity::list_user_sessions(&state.pool, tenant.id, client.user_id)
        .await?
        .into_iter()
        .map(|s: SessionRow| SessionView {
            id: s.id.to_string(),
            current: s.id == client.session.id,
            agent: agent_summary(s.user_agent.as_deref()),
            ip: s.ip.map(|i| i.to_string()).unwrap_or_default(),
            last_seen: s.last_seen_at,
            impersonated: s.impersonator_id.is_some(),
        })
        .collect();
    let (tabs, active) = account_setup(&mut ctx, client, access, "client.account.tab_security");
    Ok(SecurityPage {
        ctx,
        tabs,
        active,
        locked: credentials_locked(client),
        has_password: client.user.password_hash.is_some(),
        totp: client.user.totp_enabled,
        identities,
        providers,
        sessions,
        errors,
    })
}

async fn security(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ctx: PageCtx,
) -> AppResult<Response> {
    let page = security_page(
        &state,
        &tenant,
        &client,
        &access,
        ctx,
        FormErrors::default(),
    )
    .await?;
    Ok(no_store(render(&page)?))
}

#[derive(Debug, Deserialize)]
struct PasswordForm {
    #[serde(default)]
    current: String,
    #[serde(default)]
    new_password: String,
    #[serde(default)]
    confirm: String,
}

#[allow(clippy::too_many_arguments)]
async fn change_password(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    Form(f): Form<PasswordForm>,
) -> AppResult<Response> {
    require_own_session(&client)?;
    check_rate(&state.limits.login, &format!("pwchange:{}", client.user_id))?;
    let mut errors = FormErrors::default();
    if let Some(hash) = client.user.password_hash.clone() {
        let pw = f.current.clone();
        let ok = tokio::task::spawn_blocking(move || {
            hos_auth::password::verify_user_password(&pw, Some(&hash))
        })
        .await
        .map_err(AppError::internal)?;
        if !ok {
            errors.add("current", ctx.t("client.security.current_wrong"));
        }
    }
    if f.new_password != f.confirm {
        errors.add("confirm", ctx.t("client.security.mismatch"));
    }
    let u = &client.user;
    if let Some(p) = password_problem(
        &ctx,
        &f.new_password,
        &[&u.email, &u.first_name, &u.last_name],
    )
    .await?
    {
        errors.add("new_password", p);
    }
    if errors.any() {
        let page = security_page(&state, &tenant, &client, &access, ctx, errors).await?;
        return Ok(no_store(render_status(
            http::StatusCode::UNPROCESSABLE_ENTITY,
            &page,
        )?));
    }
    let hash = hash_password(&f.new_password).await?;
    identity::set_password_hash(&state.pool, tenant.id, client.user_id, Some(&hash)).await?;
    let ended = identity::delete_user_sessions(
        &state.pool,
        tenant.id,
        client.user_id,
        Some(client.session.id),
    )
    .await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.password.changed",
        None,
    )
    .await;
    Ok(flash::redirect(
        &ctx.url("/client/account/security"),
        SetFlash::success(ctx.tn("client.security.password_changed", ended as i64)),
    ))
}

async fn unlink(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(provider): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    require_own_session(&client)?;
    let provider: OAuthProvider = provider.parse().map_err(|_| AppError::NotFound)?;
    let back = ctx.url("/client/account/security");
    let ids = identity::list_identities(&state.pool, tenant.id, client.user_id).await?;
    if !ids.iter().any(|i| i.provider == provider) {
        return Err(AppError::NotFound);
    }
    if client.user.password_hash.is_none() && ids.len() <= 1 {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.security.last_method")),
        ));
    }
    identity::unlink_identity(&state.pool, tenant.id, client.user_id, provider).await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.identity.unlinked",
        None,
    )
    .await;
    Ok(flash::redirect(
        &back,
        SetFlash::success(ctx.t("client.security.unlinked")),
    ))
}

async fn revoke_session(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    require_own_session(&client)?;
    let id: SessionId = parse_id(&id)?;
    let back = ctx.url("/client/account/security");
    if id == client.session.id {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.security.current_session")),
        ));
    }
    let session = identity::list_user_sessions(&state.pool, tenant.id, client.user_id)
        .await?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or(AppError::NotFound)?;
    identity::delete_session(&state.pool, &session.token_hash).await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.session.revoked",
        None,
    )
    .await;
    Ok(flash::redirect(
        &back,
        SetFlash::success(ctx.t("client.security.session_revoked")),
    ))
}

async fn revoke_others(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
) -> AppResult<Response> {
    require_own_session(&client)?;
    let n = identity::delete_user_sessions(
        &state.pool,
        tenant.id,
        client.user_id,
        Some(client.session.id),
    )
    .await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.session.revoked_all",
        None,
    )
    .await;
    Ok(flash::redirect(
        &ctx.url("/client/account/security"),
        SetFlash::success(ctx.tn("client.security.sessions_revoked", n as i64)),
    ))
}

// ---------------------------------------------------------------------------
// Users (sub-accounts)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Deserialize)]
struct InviteForm {
    #[serde(default)]
    email: String,
    #[serde(default)]
    first_name: String,
    #[serde(default)]
    last_name: String,
}

#[derive(Template)]
#[template(path = "client/users.html")]
struct UsersPage {
    ctx: PageCtx,
    tabs: Vec<crate::view::NavItem>,
    active: String,
    members: Vec<ClientMemberRow>,
    areas: Vec<(String, String)>,
    form: InviteForm,
    errors: FormErrors,
    me: UserId,
}
crate::page!(UsersPage);

impl UsersPage {
    fn granted(&self, m: &ClientMemberRow, area: &str) -> bool {
        m.link.permissions.is_empty() || m.link.permissions.iter().any(|p| p == area)
    }
}

async fn users_page(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    access: &MemberAccess,
    mut ctx: PageCtx,
    form: InviteForm,
    errors: FormErrors,
) -> AppResult<UsersPage> {
    let members = clients::client_members(&state.pool, tenant.id, client.client_id).await?;
    let (tabs, active) = account_setup(&mut ctx, client, access, "client.account.tab_users");
    let areas = MEMBER_AREAS
        .iter()
        .map(|a| ((*a).to_owned(), ctx.t(&format!("client.users.area_{a}"))))
        .collect();
    Ok(UsersPage {
        ctx,
        tabs,
        active,
        members,
        areas,
        form,
        errors,
        me: client.user_id,
    })
}

async fn users(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ctx: PageCtx,
) -> AppResult<Response> {
    let page = users_page(
        &state,
        &tenant,
        &client,
        &access,
        ctx,
        InviteForm::default(),
        FormErrors::default(),
    )
    .await?;
    render(&page)
}

fn selected_areas(pairs: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| k == "perm")
        .map(|(_, v)| v.clone())
        .filter(|v| MEMBER_AREAS.contains(&v.as_str()))
        .collect();
    out.sort();
    out.dedup();
    out
}

#[allow(clippy::too_many_arguments)]
async fn invite(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    Form(pairs): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    check_rate(&state.limits.register, &ip.key("client-invite"))?;
    let get = |k: &str| {
        pairs
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let form = InviteForm {
        email: forms::normalize_email(&get("email")),
        first_name: forms::clean(&get("first_name"), 100),
        last_name: forms::clean(&get("last_name"), 100),
    };
    let mut perms = selected_areas(&pairs);
    if perms.is_empty() {
        // Nothing ticked: least privilege (support only), never "everything".
        perms.push("tickets".into());
    }
    let mut errors = FormErrors::default();
    if !forms::valid_email(&form.email) {
        errors.add("email", ctx.t("client.common.email_invalid"));
    }
    let existing = identity::user_by_email(&state.pool, tenant.id, &form.email).await?;
    if existing
        .as_ref()
        .is_some_and(|u| u.kind != UserKind::Client)
    {
        errors.add("email", ctx.t("client.users.not_client"));
    }
    if let Some(u) = &existing
        && clients::client_membership(&state.pool, tenant.id, client.client_id, u.id)
            .await?
            .is_some()
    {
        errors.add("email", ctx.t("client.users.already_member"));
    }
    if existing.is_none() && (form.first_name.is_empty() || form.last_name.is_empty()) {
        errors.add("first_name", ctx.t("client.users.name_required"));
    }
    if errors.any() {
        let page = users_page(&state, &tenant, &client, &access, ctx, form, errors).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let (user, created) = match existing {
        Some(u) => (u, false),
        None => (
            identity::create_user(
                &state.pool,
                tenant.id,
                &NewUser {
                    email: form.email.clone(),
                    password_hash: None,
                    kind: UserKind::Client,
                    first_name: form.first_name.clone(),
                    last_name: form.last_name.clone(),
                    phone: None,
                    locale: ctx.locale,
                    email_verified: false,
                },
            )
            .await?,
            true,
        ),
    };
    clients::link_invited_user(
        &state.pool,
        tenant.id,
        client.client_id,
        user.id,
        &perms,
        client.user_id,
    )
    .await?;
    if created {
        // The "set your password" link doubles as the invitation.
        crate::routes::auth::password::send_reset_link(&state, &tenant, &ctx, &user, Some(ip.0))
            .await?;
    }
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.user.invited",
        Some(("user", user.id.into_uuid())),
    )
    .await;
    Ok(flash::redirect(
        &ctx.url("/client/account/users"),
        SetFlash::success(ctx.tv("client.users.invited", "email", &user.email)),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn set_permissions(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(uid): Path<String>,
    ctx: PageCtx,
    Form(pairs): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let uid: UserId = parse_id(&uid)?;
    let mut perms = selected_areas(&pairs);
    if perms.is_empty() {
        perms.push("tickets".into());
    }
    if !clients::set_member_permissions(&state.pool, tenant.id, client.client_id, uid, &perms)
        .await?
    {
        return Err(AppError::NotFound);
    }
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.user.permissions",
        Some(("user", uid.into_uuid())),
    )
    .await;
    Ok(flash::redirect(
        &ctx.url("/client/account/users"),
        SetFlash::success(ctx.t("client.users.permissions_saved")),
    ))
}

async fn remove_user(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(uid): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let uid: UserId = parse_id(&uid)?;
    let member = clients::client_membership(&state.pool, tenant.id, client.client_id, uid)
        .await?
        .ok_or(AppError::NotFound)?;
    if member.is_owner || uid == client.user_id {
        return Ok(flash::redirect(
            &ctx.url("/client/account/users"),
            SetFlash::error(ctx.t("client.users.cannot_remove_owner")),
        ));
    }
    clients::unlink_user(&state.pool, tenant.id, client.client_id, uid).await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.user.removed",
        Some(("user", uid.into_uuid())),
    )
    .await;
    Ok(flash::redirect(
        &ctx.url("/client/account/users"),
        SetFlash::success(ctx.t("client.users.removed")),
    ))
}

// ---------------------------------------------------------------------------
// Contacts
// ---------------------------------------------------------------------------

/// E-mail categories a contact can receive.
pub const EMAIL_KINDS: &[&str] = &["invoices", "services", "domains", "support", "marketing"];

#[derive(Debug, Default, Clone, Deserialize)]
struct ContactForm {
    first_name: String,
    last_name: String,
    email: String,
    phone: String,
    company_name: String,
}

#[derive(Template)]
#[template(path = "client/contacts.html")]
struct ContactsPage {
    ctx: PageCtx,
    tabs: Vec<crate::view::NavItem>,
    active: String,
    contacts: Vec<ContactRow>,
    kinds: Vec<(String, String)>,
    form: ContactForm,
    errors: FormErrors,
}
crate::page!(ContactsPage);

async fn contacts_page(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    access: &MemberAccess,
    mut ctx: PageCtx,
    form: ContactForm,
    errors: FormErrors,
) -> AppResult<ContactsPage> {
    let contacts = clients::list_contacts(&state.pool, tenant.id, client.client_id).await?;
    let (tabs, active) = account_setup(&mut ctx, client, access, "client.account.tab_contacts");
    let kinds = EMAIL_KINDS
        .iter()
        .map(|k| ((*k).to_owned(), ctx.t(&format!("client.contacts.kind_{k}"))))
        .collect();
    Ok(ContactsPage {
        ctx,
        tabs,
        active,
        contacts,
        kinds,
        form,
        errors,
    })
}

async fn contacts(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ctx: PageCtx,
) -> AppResult<Response> {
    let page = contacts_page(
        &state,
        &tenant,
        &client,
        &access,
        ctx,
        ContactForm::default(),
        FormErrors::default(),
    )
    .await?;
    render(&page)
}

async fn contact_add(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ctx: PageCtx,
    Form(pairs): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    let get = |k: &str| {
        pairs
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let form = ContactForm {
        first_name: forms::clean(&get("first_name"), 100),
        last_name: forms::clean(&get("last_name"), 100),
        email: forms::normalize_email(&get("email")),
        phone: forms::clean(&get("phone"), 32),
        company_name: forms::clean(&get("company_name"), 200),
    };
    let mut errors = FormErrors::default();
    if form.first_name.is_empty() {
        errors.add("first_name", ctx.t("client.common.required"));
    }
    if form.last_name.is_empty() {
        errors.add("last_name", ctx.t("client.common.required"));
    }
    if !forms::valid_email(&form.email) {
        errors.add("email", ctx.t("client.common.email_invalid"));
    }
    if errors.any() {
        let page = contacts_page(&state, &tenant, &client, &access, ctx, form, errors).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let prefs: Vec<String> = pairs
        .iter()
        .filter(|(k, v)| k == "pref" && EMAIL_KINDS.contains(&v.as_str()))
        .map(|(_, v)| v.clone())
        .collect();
    let c = clients::get_client(&state.pool, tenant.id, client.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    clients::create_contact(
        &state.pool,
        tenant.id,
        client.client_id,
        &ContactInput {
            first_name: form.first_name,
            last_name: form.last_name,
            company_name: (!form.company_name.is_empty()).then_some(form.company_name),
            email: form.email,
            phone: (!form.phone.is_empty()).then_some(form.phone),
            address_line1: None,
            address_line2: None,
            city: None,
            state: None,
            postal_code: None,
            country_code: c.country_code,
            is_billing: prefs.iter().any(|p| p == "invoices"),
            is_domain_contact: prefs.iter().any(|p| p == "domains"),
            email_preferences: prefs,
        },
    )
    .await?;
    Ok(flash::redirect(
        &ctx.url("/client/account/contacts"),
        SetFlash::success(ctx.t("client.contacts.added")),
    ))
}

async fn contact_delete(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id: ContactId = parse_id(&id)?;
    let own = clients::list_contacts(&state.pool, tenant.id, client.client_id)
        .await?
        .iter()
        .any(|c| c.id == id);
    if !own || !clients::delete_contact(&state.pool, tenant.id, id).await? {
        return Err(AppError::NotFound);
    }
    Ok(flash::redirect(
        &ctx.url("/client/account/contacts"),
        SetFlash::success(ctx.t("client.contacts.deleted")),
    ))
}

// ---------------------------------------------------------------------------
// API keys
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "client/api_keys.html")]
struct ApiKeysPage {
    ctx: PageCtx,
    tabs: Vec<crate::view::NavItem>,
    active: String,
    keys: Vec<ApiKeyRow>,
    secret: String,
    locked: bool,
    error: String,
}
crate::page!(ApiKeysPage);

async fn api_page(
    state: &AppState,
    tenant: &Tenant,
    client: &CurrentClient,
    access: &MemberAccess,
    mut ctx: PageCtx,
    secret: String,
    error: String,
) -> AppResult<ApiKeysPage> {
    let keys = identity::list_api_keys(&state.pool, tenant.id, Some(client.user_id))
        .await?
        .into_iter()
        .filter(|k| k.revoked_at.is_none())
        .collect();
    let (tabs, active) = account_setup(&mut ctx, client, access, "client.account.tab_api");
    Ok(ApiKeysPage {
        ctx,
        tabs,
        active,
        keys,
        secret,
        locked: credentials_locked(client),
        error,
    })
}

async fn api_keys(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ctx: PageCtx,
) -> AppResult<Response> {
    let page = api_page(
        &state,
        &tenant,
        &client,
        &access,
        ctx,
        String::new(),
        String::new(),
    )
    .await?;
    Ok(no_store(render(&page)?))
}

#[derive(Debug, Deserialize)]
struct ApiKeyForm {
    #[serde(default)]
    name: String,
}

#[allow(clippy::too_many_arguments)]
async fn api_key_create(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    Form(f): Form<ApiKeyForm>,
) -> AppResult<Response> {
    require_own_session(&client)?;
    // An account whose e-mail is unverified may have been pre-registered by
    // someone else: no long-lived credentials until the mailbox is proven.
    if client.user.email_verified_at.is_none() {
        let error = ctx.t("client.api.verify_first");
        let page = api_page(&state, &tenant, &client, &access, ctx, String::new(), error).await?;
        return Ok(no_store(render_status(http::StatusCode::FORBIDDEN, &page)?));
    }
    let name = forms::clean(&f.name, 80);
    if name.is_empty() {
        let page = api_page(
            &state,
            &tenant,
            &client,
            &access,
            ctx,
            String::new(),
            String::new(),
        )
        .await?;
        let mut res = render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page)?;
        res.headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return Ok(res);
    }
    let generated = hos_auth::tokens::generate_api_key();
    let key = identity::create_api_key(
        &state.pool,
        tenant.id,
        Some(client.user_id),
        &name,
        &generated.hint,
        &generated.key_hash,
        &[],
        None,
    )
    .await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.api_key.created",
        Some(("api_key", key.id.into_uuid())),
    )
    .await;
    let secret = generated.key.expose_str().to_owned();
    let page = api_page(
        &state,
        &tenant,
        &client,
        &access,
        ctx,
        secret,
        String::new(),
    )
    .await?;
    Ok(no_store(render(&page)?))
}

async fn api_key_revoke(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id: ApiKeyId = parse_id(&id)?;
    let own = identity::list_api_keys(&state.pool, tenant.id, Some(client.user_id))
        .await?
        .iter()
        .any(|k| k.id == id);
    if !own {
        return Err(AppError::NotFound);
    }
    identity::revoke_api_key(&state.pool, tenant.id, id).await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.api_key.revoked",
        Some(("api_key", id.into_uuid())),
    )
    .await;
    Ok(flash::redirect(
        &ctx.url("/client/account/api-keys"),
        SetFlash::success(ctx.t("client.api.revoked")),
    ))
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "client/notifications.html")]
struct NotificationsPage {
    ctx: PageCtx,
    tabs: Vec<crate::view::NavItem>,
    active: String,
    marketing: bool,
    overdue: bool,
}
crate::page!(NotificationsPage);

async fn notifications(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let c = clients::get_client(&state.pool, tenant.id, client.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let (tabs, active) = account_setup(
        &mut ctx,
        &client,
        &access,
        "client.account.tab_notifications",
    );
    render(&NotificationsPage {
        ctx,
        tabs,
        active,
        marketing: c.marketing_opt_in,
        overdue: c.overdue_notices,
    })
}

#[derive(Debug, Deserialize)]
struct NotificationsForm {
    marketing: Option<String>,
}

async fn notifications_save(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    Form(f): Form<NotificationsForm>,
) -> AppResult<Response> {
    let c = clients::get_client(&state.pool, tenant.id, client.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let input = clients::ClientInput {
        kind: c.kind,
        first_name: c.first_name.clone(),
        last_name: c.last_name.clone(),
        company_name: c.company_name.clone(),
        email: c.email.clone(),
        phone: c.phone.clone(),
        address_line1: c.address_line1.clone(),
        address_line2: c.address_line2.clone(),
        city: c.city.clone(),
        state: c.state.clone(),
        postal_code: c.postal_code.clone(),
        country_code: c.country_code.clone(),
        tax_office: c.tax_office.clone(),
        tax_number: c.tax_number.clone(),
        national_id: c.national_id.clone(),
        currency: c.currency,
        locale: c.locale,
        group_id: c.group_id,
        tax_exempt: c.tax_exempt,
        marketing_opt_in: forms::checked(f.marketing.as_deref()),
    };
    clients::update_client(&state.pool, tenant.id, client.client_id, &input).await?;
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.notifications.updated",
        None,
    )
    .await;
    Ok(flash::redirect(
        &ctx.url("/client/account/notifications"),
        SetFlash::success(ctx.t("client.account.saved")),
    ))
}

// ---------------------------------------------------------------------------
// Activity
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "client/activity.html")]
struct ActivityPage {
    ctx: PageCtx,
    tabs: Vec<crate::view::NavItem>,
    active: String,
    rows: Vec<AuditLogRow>,
}
crate::page!(ActivityPage);

impl ActivityPage {
    fn label(&self, action: &str) -> String {
        let key = format!("client.activity.{}", action.replace('.', "_"));
        if self.cat.has(&key) {
            self.t(&key)
        } else {
            action.to_owned()
        }
    }
}

async fn activity(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    access: MemberAccess,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let rows = audit::list(
        &state.pool,
        tenant.id,
        &AuditFilter {
            actor_id: Some(client.user_id.into_uuid()),
            ..AuditFilter::default()
        },
        Page::first(50),
    )
    .await?
    .items;
    let (tabs, active) = account_setup(&mut ctx, &client, &access, "client.account.tab_activity");
    render(&ActivityPage {
        ctx,
        tabs,
        active,
        rows,
    })
}

// ---------------------------------------------------------------------------
// Privacy (KVKK / GDPR)
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "client/privacy.html")]
struct PrivacyPage {
    ctx: PageCtx,
    tabs: Vec<crate::view::NavItem>,
    active: String,
}
crate::page!(PrivacyPage);

async fn privacy(
    client: CurrentClient,
    access: MemberAccess,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let (tabs, active) = account_setup(&mut ctx, &client, &access, "client.account.tab_privacy");
    render(&PrivacyPage { ctx, tabs, active })
}

/// `GET /client/account/privacy/export`: the account's personal data as a
/// JSON download (no secrets: no password hashes, keys or credentials).
async fn export(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
) -> AppResult<Response> {
    let pool = &state.pool;
    let c = clients::get_client(pool, tenant.id, client.client_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let members = clients::client_members(pool, tenant.id, c.id).await?;
    let contacts = clients::list_contacts(pool, tenant.id, c.id).await?;
    let services = service_repo::client_services(pool, tenant.id, c.id).await?;
    let domains = domain_repo::client_domains(pool, tenant.id, c.id).await?;
    let invoices = invoices::list_invoices(
        pool,
        tenant.id,
        &invoices::InvoiceFilter {
            client_id: Some(c.id),
            ..invoices::InvoiceFilter::default()
        },
        Page::first(1000),
    )
    .await?
    .items;
    let tickets = support::list_tickets(
        pool,
        tenant.id,
        &support::TicketFilter {
            client_id: Some(c.id),
            ..support::TicketFilter::default()
        },
        Page::first(1000),
    )
    .await?
    .items;
    let doc = json!({
        "exported_at": Utc::now(),
        "client": {
            "id": c.id, "kind": c.kind, "first_name": c.first_name, "last_name": c.last_name,
            "company_name": c.company_name, "email": c.email, "phone": c.phone,
            "address_line1": c.address_line1, "address_line2": c.address_line2, "city": c.city,
            "state": c.state, "postal_code": c.postal_code, "country_code": c.country_code,
            "tax_office": c.tax_office, "tax_number": c.tax_number, "national_id": c.national_id,
            "currency": c.currency, "locale": c.locale, "marketing_opt_in": c.marketing_opt_in,
            "created_at": c.created_at,
        },
        "users": members.iter().map(|m| json!({
            "email": m.email, "first_name": m.first_name, "last_name": m.last_name,
            "owner": m.link.is_owner, "permissions": m.link.permissions, "last_login_at": m.last_login_at,
        })).collect::<Vec<_>>(),
        "contacts": contacts.iter().map(|x| json!({
            "first_name": x.first_name, "last_name": x.last_name, "email": x.email, "phone": x.phone,
            "company_name": x.company_name, "email_preferences": x.email_preferences,
        })).collect::<Vec<_>>(),
        "services": services.iter().map(|s| json!({
            "id": s.service.id, "product": s.product_name, "status": s.service.status,
            "domain": s.service.domain, "billing_cycle": s.service.billing_cycle,
            "recurring_amount": s.service.recurring_amount, "currency": s.service.currency,
            "registration_date": s.service.registration_date, "next_due_date": s.service.next_due_date,
        })).collect::<Vec<_>>(),
        "domains": domains.iter().map(|d| json!({
            "name": d.name, "status": d.status, "registered_at": d.registered_at,
            "expires_at": d.expires_at, "auto_renew": d.auto_renew, "nameservers": d.nameservers,
        })).collect::<Vec<_>>(),
        "invoices": invoices.iter().map(|i| json!({
            "number": i.invoice.number, "status": i.invoice.status, "issue_date": i.invoice.issue_date,
            "due_date": i.invoice.due_date, "total": i.invoice.total, "currency": i.invoice.currency,
            "billing": i.invoice.billing_snapshot,
        })).collect::<Vec<_>>(),
        "tickets": tickets.iter().map(|t| json!({
            "number": t.ticket.number, "subject": t.ticket.subject, "status": t.ticket.status,
            "created_at": t.ticket.created_at,
        })).collect::<Vec<_>>(),
    });
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.data.exported",
        None,
    )
    .await;
    let body = serde_json::to_vec_pretty(&doc).map_err(AppError::internal)?;
    let mut res = body.into_response();
    let h = res.headers_mut();
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    h.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"hesap-verileri.json\""),
    );
    h.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok(res)
}

#[derive(Debug, Deserialize)]
struct DeleteForm {
    #[serde(default)]
    reason: String,
    confirm: Option<String>,
}

/// `POST /client/account/privacy/delete`: files the deletion request as a
/// support ticket (staff verify open balances / legal retention first).
async fn delete_request(
    State(state): State<AppState>,
    tenant: Tenant,
    client: CurrentClient,
    ip: ClientIp,
    ua: UserAgent,
    ctx: PageCtx,
    Form(f): Form<DeleteForm>,
) -> AppResult<Response> {
    let back = ctx.url("/client/account/privacy");
    if !forms::checked(f.confirm.as_deref()) {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.common.confirm_required")),
        ));
    }
    let Some(dept) = support::list_departments(&state.pool, tenant.id, true)
        .await?
        .into_iter()
        .next()
    else {
        return Ok(flash::redirect(
            &back,
            SetFlash::error(ctx.t("client.privacy.no_department")),
        ));
    };
    let reason = forms::clean(&f.reason, 2000);
    let body = format!(
        "{}\n\n{}",
        ctx.t("client.privacy.ticket_body"),
        if reason.is_empty() {
            "—".to_owned()
        } else {
            reason
        }
    );
    let mut conn = state.pool.acquire().await?;
    let (ticket, _) = support::open_ticket(
        &mut conn,
        tenant.id,
        &support::NewTicket {
            department_id: dept.id,
            client_id: Some(client.client_id),
            user_id: Some(client.user_id),
            guest_name: None,
            guest_email: None,
            subject: ctx.t("client.privacy.ticket_subject"),
            priority: TicketPriority::High,
            service_id: None,
            domain_id: None,
            first_message: support::NewTicketMessage {
                author_kind: AuthorKind::Client,
                user_id: Some(client.user_id),
                author_name: format!("{} {}", client.user.first_name, client.user.last_name),
                author_email: Some(client.user.email.clone()),
                body,
                is_internal: false,
                source: MessageSource::Web,
                message_id: None,
                ip: Some(ip.0),
            },
        },
    )
    .await?;
    drop(conn);
    audit_client(
        &state,
        &tenant,
        &client,
        ip,
        &ua,
        "client.deletion.requested",
        Some(("ticket", ticket.id.into_uuid())),
    )
    .await;
    Ok(flash::redirect(
        &ctx.url(format!("/client/tickets/{}", ticket.id)),
        SetFlash::success(ctx.t("client.privacy.requested")),
    ))
}

#[cfg(test)]
mod tests {
    use super::agent_summary;

    #[test]
    fn agents() {
        assert_eq!(
            agent_summary(Some(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Safari/605.1.15"
            )),
            "Safari · macOS"
        );
        assert_eq!(agent_summary(None), "—");
    }
}
