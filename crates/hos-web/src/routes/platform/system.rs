//! System pages of the console: platform settings, operators, the
//! operator's own 2FA, the global job queue / scheduled tasks, the global
//! audit log and version / health information.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use chrono::Utc;
use hos_auth::totp::{TotpSecret, generate_recovery_codes, otpauth_uri, verify_code};
use hos_core::ids::{JobId, PlatformAdminId, TenantId};
use hos_db::enums::{AccountStatus, ActorKind, JobStatus};
use hos_db::repo::jobs::{self as jobs_repo, JobFilter, JobRow};
use hos_db::repo::platform::{self, PlatformAdminRow};
use hos_db::repo::saas::{
    self, GlobalAuditFilter, GlobalAuditRow, JobHealth, MigrationStatus, ScheduledTaskListRow,
};
use serde::Deserialize;
use serde_json::json;

use super::login::{check_admin_code, totp_aad};
use super::{AuditCtx, PlatformSettings, audit_platform, menu};
use crate::error::{AppError, AppResult};
use crate::extract::{PageQuery, PlatformAdmin};
use crate::flash::{self, SetFlash};
use crate::forms::{self, FormErrors};
use crate::routes::auth::two_factor::qr_svg;
use crate::state::AppState;
use crate::view::{Crumb, PageCtx, Pager, render, render_status};

fn chrome(ctx: &mut PageCtx, key: &str) {
    let title = ctx.t(key);
    ctx.set_title(title.clone());
    ctx.panel = Some(menu::chrome(ctx, vec![Crumb::current(title)]));
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SettingsForm {
    #[serde(default)]
    signup_enabled: Option<String>,
    #[serde(default)]
    trial_days: String,
    #[serde(default)]
    default_plan: String,
}

#[derive(Template)]
#[template(path = "platform/settings.html")]
struct SettingsPage {
    ctx: PageCtx,
    signup_enabled: bool,
    trial_days: String,
    default_plan: String,
    plans: Vec<(String, String)>,
    errors: FormErrors,
    signup_url: String,
}
crate::page!(SettingsPage);

async fn settings_page(
    state: &AppState,
    mut ctx: PageCtx,
    settings: &PlatformSettings,
    trial_days: String,
    errors: FormErrors,
) -> AppResult<SettingsPage> {
    chrome(&mut ctx, "platform.settings.title");
    let plans = saas::list_all_plans(&state.pool).await?;
    let mut options = vec![(String::new(), ctx.t("platform.settings.no_plan"))];
    options.extend(super::tenants::plan_options(&ctx, &plans));
    Ok(SettingsPage {
        signup_enabled: settings.signup_enabled,
        trial_days,
        default_plan: settings.default_plan.clone().unwrap_or_default(),
        plans: options,
        errors,
        signup_url: format!(
            "{}/platform/signup",
            state.config.base_url.as_str().trim_end_matches('/')
        ),
        ctx,
    })
}

/// `GET /platform/settings`.
pub async fn settings(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    ctx: PageCtx,
) -> AppResult<Response> {
    let s = PlatformSettings::load(&state).await?;
    let days = s.trial_days.to_string();
    render(&settings_page(&state, ctx, &s, days, FormErrors::default()).await?)
}

/// `POST /platform/settings`.
pub async fn save_settings(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    ctx: PageCtx,
    Form(form): Form<SettingsForm>,
) -> AppResult<Response> {
    let mut errors = FormErrors::default();
    let trial_days = match form.trial_days.trim().parse::<i64>() {
        Ok(n) if (0..=365).contains(&n) => n,
        _ => {
            errors.add("trial_days", ctx.t("platform.errors.number"));
            0
        }
    };
    let plan_code = form.default_plan.trim().to_owned();
    if !plan_code.is_empty()
        && platform::get_plan_by_code(&state.pool, &plan_code)
            .await?
            .is_none()
    {
        errors.add("default_plan", ctx.t("platform.signup.plan_invalid"));
    }
    let settings = PlatformSettings {
        signup_enabled: forms::checked(form.signup_enabled.as_deref()),
        trial_days,
        default_plan: (!plan_code.is_empty()).then_some(plan_code),
    };
    if settings.signup_enabled
        && super::signup_blocked_by_policy(hos_core::net::OutboundPolicy::global(), state.web.debug)
    {
        errors.add(
            "signup_enabled",
            ctx.t("platform.settings.signup_policy_private"),
        );
    }
    if errors.any() {
        let page = settings_page(&state, ctx, &settings, form.trial_days, errors).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    settings.save(&state).await?;
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.settings.updated",
        None,
        json!({
            "signup_enabled": settings.signup_enabled,
            "trial_days": settings.trial_days,
            "default_plan": settings.default_plan,
        }),
    )
    .await;
    Ok(flash::redirect(
        "/platform/settings",
        SetFlash::success(ctx.t("platform.common.saved")),
    ))
}

// ---------------------------------------------------------------------------
// operators
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AdminForm {
    #[serde(default)]
    email: String,
    #[serde(default)]
    name: String,
}

#[derive(Template)]
#[template(path = "platform/admins.html")]
struct AdminsPage {
    ctx: PageCtx,
    admins: Vec<PlatformAdminRow>,
    me: PlatformAdminId,
    form: AdminForm,
    errors: FormErrors,
    created_email: String,
    created_password: String,
}
crate::page!(AdminsPage);

async fn admins_page(
    state: &AppState,
    mut ctx: PageCtx,
    me: PlatformAdminId,
    form: AdminForm,
    errors: FormErrors,
    created: Option<(String, String)>,
) -> AppResult<AdminsPage> {
    chrome(&mut ctx, "platform.admins.title");
    let admins = saas::list_platform_admins(&state.pool).await?;
    let (created_email, created_password) = created.unwrap_or_default();
    Ok(AdminsPage {
        ctx,
        admins,
        me,
        form,
        errors,
        created_email,
        created_password,
    })
}

/// `GET /platform/admins`.
pub async fn admins(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    ctx: PageCtx,
) -> AppResult<Response> {
    render(
        &admins_page(
            &state,
            ctx,
            admin.admin.id,
            AdminForm::default(),
            FormErrors::default(),
            None,
        )
        .await?,
    )
}

/// `POST /platform/admins` — creates an operator with a random password,
/// shown once.
pub async fn add_admin(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    ctx: PageCtx,
    Form(form): Form<AdminForm>,
) -> AppResult<Response> {
    let mut errors = FormErrors::default();
    let email = forms::normalize_email(&form.email);
    if !forms::valid_email(&email) {
        errors.add("email", ctx.t("auth.errors.email_invalid"));
    }
    let name = forms::clean(&form.name, 100);
    if name.is_empty() {
        errors.add("name", ctx.t("platform.errors.required"));
    }
    if errors.any() {
        let page = admins_page(&state, ctx, admin.admin.id, form, errors, None).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    let password = crate::cli::random_password();
    if !crate::cli::create_platform_admin(&state.pool, &email, &name, &password).await? {
        errors.add("email", ctx.t("platform.admins.exists"));
        let page = admins_page(&state, ctx, admin.admin.id, form, errors, None).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.admin.created",
        None,
        json!({ "email": email }),
    )
    .await;
    render(
        &admins_page(
            &state,
            ctx,
            admin.admin.id,
            AdminForm::default(),
            FormErrors::default(),
            Some((email, password)),
        )
        .await?,
    )
}

/// `POST /platform/admins/{id}/delete` — not yourself, never the last one.
pub async fn delete_admin(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id: PlatformAdminId = id.parse().map_err(|_| AppError::NotFound)?;
    if id == admin.admin.id {
        return Ok(flash::redirect(
            "/platform/admins",
            SetFlash::error(ctx.t("platform.admins.not_self")),
        ));
    }
    let target = platform::get_platform_admin(&state.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut conn = state.pool.acquire().await?;
    match saas::delete_platform_admin(&mut conn, id).await {
        Ok(_) => {}
        Err(hos_core::CoreError::Conflict(_)) => {
            return Ok(flash::redirect(
                "/platform/admins",
                SetFlash::error(ctx.t("platform.admins.last")),
            ));
        }
        Err(e) => return Err(e.into()),
    }
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.admin.deleted",
        Some(("platform_admin", id.into())),
        json!({ "email": target.email }),
    )
    .await;
    Ok(flash::redirect(
        "/platform/admins",
        SetFlash::success(ctx.t("platform.admins.deleted")),
    ))
}

// ---------------------------------------------------------------------------
// own account: TOTP 2FA
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "platform/account.html")]
struct AccountPage {
    ctx: PageCtx,
    admin: PlatformAdminRow,
    enabled: bool,
    qr: String,
    secret: String,
    recovery_left: usize,
    codes: Vec<String>,
    codes_text: String,
    errors: FormErrors,
}
crate::page!(AccountPage);

fn grouped(secret: &str) -> String {
    secret
        .chars()
        .collect::<Vec<_>>()
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds the account page; when 2FA is off a fresh pending secret is stored
/// (unless `keep_pending` and one exists).
async fn account_page(
    state: &AppState,
    mut ctx: PageCtx,
    admin: &PlatformAdminRow,
    errors: FormErrors,
    keep_pending: bool,
    codes: Vec<String>,
) -> AppResult<AccountPage> {
    chrome(&mut ctx, "platform.account.title");
    let totp = saas::platform_admin_totp(&state.pool, admin.id)
        .await?
        .ok_or(AppError::NotFound)?;
    if totp.totp_enabled {
        return Ok(AccountPage {
            ctx,
            admin: admin.clone(),
            enabled: true,
            qr: String::new(),
            secret: String::new(),
            recovery_left: totp.totp_recovery_hashes.len(),
            codes_text: codes.join("\n"),
            codes,
            errors,
        });
    }
    let secret = match (&totp.totp_secret_enc, keep_pending) {
        (Some(enc), true) => TotpSecret::from_bytes(
            state
                .secret_box
                .decrypt(enc, &totp_aad(admin.id))
                .map_err(AppError::internal)?,
        )?,
        _ => {
            let secret = TotpSecret::generate();
            let enc = state
                .secret_box
                .encrypt(secret.as_bytes(), &totp_aad(admin.id))
                .map_err(AppError::internal)?;
            saas::set_platform_admin_totp(&state.pool, admin.id, Some(&enc), false, &[]).await?;
            secret
        }
    };
    let uri = otpauth_uri(&secret, "HostingOS Platform", &admin.email)?;
    Ok(AccountPage {
        qr: qr_svg(&uri)?,
        secret: grouped(secret.to_base32().expose_str()),
        ctx,
        admin: admin.clone(),
        enabled: false,
        recovery_left: 0,
        codes_text: String::new(),
        codes: Vec::new(),
        errors,
    })
}

/// `GET /platform/account`.
pub async fn account(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    ctx: PageCtx,
) -> AppResult<Response> {
    render(
        &account_page(
            &state,
            ctx,
            &admin.admin,
            FormErrors::default(),
            false,
            Vec::new(),
        )
        .await?,
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct CodeForm {
    #[serde(default)]
    code: String,
}

/// `POST /platform/account/2fa` — confirms the pending secret with a code
/// and shows ten recovery codes once.
pub async fn confirm_two_factor(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    ctx: PageCtx,
    Form(form): Form<CodeForm>,
) -> AppResult<Response> {
    crate::state::check_rate(&state.limits.two_factor, &req.ip.key("platform-2fa-setup"))?;
    let totp = saas::platform_admin_totp(&state.pool, admin.admin.id)
        .await?
        .ok_or(AppError::NotFound)?;
    let pending = match (&totp.totp_secret_enc, totp.totp_enabled) {
        (Some(enc), false) => enc.clone(),
        _ => {
            return Ok(flash::redirect(
                "/platform/account",
                SetFlash::info(ctx.t("platform.account.already")),
            ));
        }
    };
    let secret = TotpSecret::from_bytes(
        state
            .secret_box
            .decrypt(&pending, &totp_aad(admin.admin.id))
            .map_err(AppError::internal)?,
    )?;
    let code: String = form.code.chars().filter(|c| !c.is_whitespace()).collect();
    let now = u64::try_from(Utc::now().timestamp()).unwrap_or_default();
    let Some(step) = verify_code(&secret, &code, now, None) else {
        let mut errors = FormErrors::default();
        errors.add("code", ctx.t("auth.two_factor.invalid"));
        let page = account_page(&state, ctx, &admin.admin, errors, true, Vec::new()).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    };
    let codes = generate_recovery_codes();
    saas::set_platform_admin_totp(
        &state.pool,
        admin.admin.id,
        Some(&pending),
        true,
        &codes.hashes,
    )
    .await?;
    saas::use_platform_totp_step(
        &state.pool,
        admin.admin.id,
        i64::try_from(step).map_err(AppError::internal)?,
    )
    .await?;
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.admin.2fa_enabled",
        None,
        json!({}),
    )
    .await;
    let plain: Vec<String> = codes
        .codes
        .iter()
        .map(|c| c.expose_str().to_owned())
        .collect();
    let mut admin_row = admin.admin.clone();
    admin_row.totp_enabled = true;
    let mut page =
        account_page(&state, ctx, &admin_row, FormErrors::default(), true, plain).await?;
    page.ctx.flashes.push(crate::flash::FlashMessage {
        kind: crate::flash::FlashKind::Success,
        text: page.ctx.t("auth.two_factor.enabled"),
    });
    render(&page)
}

#[derive(Debug, Default, Deserialize)]
pub struct DisableForm {
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: String,
}

/// `POST /platform/account/2fa/disable` — needs the password and a current code.
pub async fn disable_two_factor(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    ctx: PageCtx,
    Form(form): Form<DisableForm>,
) -> AppResult<Response> {
    crate::state::check_rate(
        &state.limits.two_factor,
        &req.ip.key("platform-2fa-disable"),
    )?;
    let stored = admin.admin.password_hash.clone();
    let pw = form.password.clone();
    let password_ok = tokio::task::spawn_blocking(move || {
        hos_auth::password::verify_user_password(&pw, stored.as_deref())
    })
    .await?;
    let code_ok = password_ok && check_admin_code(&state, admin.admin.id, &form.code).await?;
    if !code_ok {
        let mut errors = FormErrors::default();
        errors.add("password", ctx.t("auth.two_factor.confirm_failed"));
        let page = account_page(&state, ctx, &admin.admin, errors, true, Vec::new()).await?;
        return render_status(http::StatusCode::UNPROCESSABLE_ENTITY, &page);
    }
    saas::set_platform_admin_totp(&state.pool, admin.admin.id, None, false, &[]).await?;
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.admin.2fa_disabled",
        None,
        json!({}),
    )
    .await;
    Ok(flash::redirect(
        "/platform/account",
        SetFlash::success(ctx.t("auth.two_factor.disabled")),
    ))
}

// ---------------------------------------------------------------------------
// jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct JobsQuery {
    #[serde(default)]
    status: String,
    #[serde(default)]
    queue: String,
}

#[derive(Template)]
#[template(path = "platform/jobs.html")]
struct JobsPage {
    ctx: PageCtx,
    health: JobHealth,
    counts: Vec<(String, String, i64)>,
    jobs: Vec<JobRow>,
    pager: Pager,
    status: String,
    queue: String,
    statuses: Vec<(String, String)>,
    tasks: Vec<ScheduledTaskListRow>,
}
crate::page!(JobsPage);

impl JobsPage {
    fn tone(&self, status: &str) -> &'static str {
        match status {
            "running" => "info",
            "ok" => "success",
            other => crate::view::status_tone(other),
        }
    }
}

/// `GET /platform/jobs` — queue health, counts, jobs and scheduled tasks.
pub async fn jobs(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    q: PageQuery,
    Query(jq): Query<JobsQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    chrome(&mut ctx, "platform.jobs.title");
    let status: Option<JobStatus> = jq.status.parse().ok();
    let queue = jq.queue.trim().to_owned();
    let filter = JobFilter {
        status,
        queue: (!queue.is_empty()).then(|| queue.clone()),
        ..JobFilter::default()
    };
    let paged = jobs_repo::list_jobs(&state.pool, &filter, q.db_page()).await?;
    let health = saas::job_health(&state.pool).await?;
    let counts = jobs_repo::job_counts(&state.pool)
        .await?
        .into_iter()
        .map(|(q, s, n)| (q, s.as_str().to_owned(), n))
        .collect();
    let tasks = saas::list_all_scheduled_tasks(&state.pool, 200).await?;
    let st = status.map(|s| s.as_str().to_owned()).unwrap_or_default();
    let pager = Pager::new(
        &paged,
        "/platform/jobs",
        &[("status", &st), ("queue", &queue)],
    );
    let statuses = std::iter::once((String::new(), ctx.t("platform.jobs.all")))
        .chain(
            JobStatus::ALL
                .iter()
                .map(|s| (s.as_str().to_owned(), s.as_str().to_owned())),
        )
        .collect();
    render(&JobsPage {
        ctx,
        health,
        counts,
        jobs: paged.items,
        pager,
        status: st,
        queue,
        statuses,
        tasks,
    })
}

fn parse_job(raw: &str) -> AppResult<JobId> {
    raw.parse().map_err(|_| AppError::NotFound)
}

/// `POST /platform/jobs/{id}/retry` (failed / cancelled jobs).
pub async fn retry_job(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = parse_job(&id)?;
    let ok = jobs_repo::retry_job(&state.pool, id).await?;
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.job.retried",
        Some(("job", id.into())),
        json!({ "ok": ok }),
    )
    .await;
    let flash = if ok {
        SetFlash::success(ctx.t("platform.jobs.retried"))
    } else {
        SetFlash::error(ctx.t("platform.jobs.not_changed"))
    };
    Ok(flash::redirect("/platform/jobs", flash))
}

/// `POST /platform/jobs/{id}/cancel` (pending jobs).
pub async fn cancel_job(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    Path(id): Path<String>,
    ctx: PageCtx,
) -> AppResult<Response> {
    let id = parse_job(&id)?;
    let ok = jobs_repo::cancel_job(&state.pool, id).await?;
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.job.cancelled",
        Some(("job", id.into())),
        json!({ "ok": ok }),
    )
    .await;
    let flash = if ok {
        SetFlash::success(ctx.t("platform.jobs.cancelled"))
    } else {
        SetFlash::error(ctx.t("platform.jobs.not_changed"))
    };
    Ok(flash::redirect("/platform/jobs", flash))
}

// ---------------------------------------------------------------------------
// audit log
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    action: String,
    #[serde(default)]
    tenant: String,
    #[serde(default)]
    actor: String,
}

#[derive(Template)]
#[template(path = "platform/audit.html")]
struct AuditPage {
    ctx: PageCtx,
    rows: Vec<GlobalAuditRow>,
    pager: Pager,
    action: String,
    tenant_filter: String,
    actor: String,
    actors: Vec<(String, String)>,
}
crate::page!(AuditPage);

/// `GET /platform/audit` — every tenant's audit log plus platform entries.
pub async fn audit_log(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    q: PageQuery,
    Query(aq): Query<AuditQuery>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    chrome(&mut ctx, "platform.audit.title");
    let tenant_slug = aq.tenant.trim().to_ascii_lowercase();
    let (tenant_id, platform_only): (Option<TenantId>, bool) = match tenant_slug.as_str() {
        "" => (None, false),
        "-" => (None, true),
        slug => match platform::get_tenant_by_slug(&state.pool, slug).await? {
            Some(t) => (Some(t.id), false),
            None => (Some(TenantId::new()), false),
        },
    };
    let actor_kind: Option<ActorKind> = aq.actor.parse().ok();
    let filter = GlobalAuditFilter {
        tenant_id,
        platform_only,
        actor_kind,
        action_prefix: Some(aq.action.trim().to_owned()).filter(|a| !a.is_empty()),
    };
    let paged = saas::list_audit_global(&state.pool, &filter, q.db_page()).await?;
    let action = aq.action.trim().to_owned();
    let actor = actor_kind
        .map(|k| k.as_str().to_owned())
        .unwrap_or_default();
    let pager = Pager::new(
        &paged,
        "/platform/audit",
        &[
            ("action", &action),
            ("tenant", &tenant_slug),
            ("actor", &actor),
        ],
    );
    let actors = std::iter::once((String::new(), ctx.t("platform.audit.all_actors")))
        .chain(
            ActorKind::ALL
                .iter()
                .map(|k| (k.as_str().to_owned(), k.as_str().to_owned())),
        )
        .collect();
    render(&AuditPage {
        ctx,
        rows: paged.items,
        pager,
        action,
        tenant_filter: tenant_slug,
        actor,
        actors,
    })
}

// ---------------------------------------------------------------------------
// version / health
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "platform/system.html")]
struct SystemPage {
    ctx: PageCtx,
    version: String,
    uptime: String,
    db_ok: bool,
    db_ms: u128,
    migrations: Option<MigrationStatus>,
    jobs: JobHealth,
    base_url: String,
    base_domain: String,
    debug: bool,
    secure: bool,
    trust_proxy: bool,
    admins_active: usize,
}
crate::page!(SystemPage);

impl SystemPage {
    fn db_label(&self) -> &'static str {
        if self.db_ok {
            "platform.system.db_ok"
        } else {
            "platform.system.db_down"
        }
    }
}

fn human_duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d {h}h {m}m")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m {}s", secs % 60)
    }
}

/// `GET /platform/system`.
pub async fn health(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    chrome(&mut ctx, "platform.system.title");
    let started = std::time::Instant::now();
    let db_ok = sqlx::query_scalar::<_, i32>("select 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();
    let db_ms = started.elapsed().as_millis();
    let migrations = saas::migration_status(&state.pool).await.ok();
    let jobs = saas::job_health(&state.pool).await?;
    let admins_active = saas::list_platform_admins(&state.pool)
        .await?
        .iter()
        .filter(|a| a.status == AccountStatus::Active)
        .count();
    render(&SystemPage {
        ctx,
        // "0.1.0 · abc123def456" (details on /platform/updates).
        version: format!(
            "{} · {}",
            state.updates.build.version,
            state.updates.build.short_commit()
        ),
        uptime: human_duration(state.started_at.elapsed().as_secs()),
        db_ok,
        db_ms,
        migrations,
        jobs,
        base_url: state.config.base_url.to_string(),
        base_domain: state.config.base_domain.clone(),
        debug: state.web.debug,
        secure: state.web.secure,
        trust_proxy: state.web.trust_proxy,
        admins_active,
    })
}
