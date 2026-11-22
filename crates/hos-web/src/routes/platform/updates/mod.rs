//! `/platform/updates` — version, available releases, install / deploy,
//! progress that survives the restart, history and settings
//! (docs/UPDATES.md). Platform admins only; every state change needs the
//! CSRF token, installs and deploys also the password (+ TOTP code when 2FA
//! is on), and each is written to the audit log.
//!
//! | Route | |
//! |---|---|
//! | `GET /platform/updates` | overview |
//! | `POST /platform/updates/check` | check now |
//! | `POST /platform/updates/settings` | channel, automatic checks, e-mail, deploy webhook |
//! | `POST /platform/updates/apply` | install in place (bare binary) |
//! | `POST /platform/updates/deploy` | call the deploy webhook (containers) |
//! | `POST /platform/updates/maintenance/off` | end a stuck maintenance mode |
//! | `GET /platform/updates/runs/{id}` (+ `/status` fragment) | progress / result |
//! | `POST /platform/updates/notifications/{id}/dismiss` | hide a notice |

pub mod maintenance;
pub mod runtime;

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse as _, Redirect, Response};
use chrono::{DateTime, Utc};
use hos_db::repo::updates::{self as repo, RunRow, SettingsInput};
use hos_update::backup::BackupPlan;
use hos_update::install::InstallKind;
use hos_update::{Channel, UpdateError};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{AuditCtx, audit_platform, menu};
use crate::error::{AppError, AppResult};
use crate::extract::{Htmx, PlatformAdmin};
use crate::flash::{self, SetFlash};
use crate::forms;
use crate::state::{AppState, check_rate};
use crate::view::{Crumb, PageCtx, render};

pub use runtime::UpdateRuntime;

const BASE: &str = "/platform/updates";

// ---------------------------------------------------------------------------
// view models
// ---------------------------------------------------------------------------

/// A release on the page.
#[derive(Debug, Clone)]
pub struct ReleaseView {
    pub version: String,
    pub security: bool,
    pub verified: bool,
    pub trust_detail: String,
    pub published_at: DateTime<Utc>,
    /// Release notes rendered from Markdown (verified releases only).
    pub notes_html: String,
    /// Release notes of an unverified release: untrusted, shown as plain
    /// text (escaped by the template, no links or images).
    pub notes_text: String,
    pub migrations: bool,
    pub rollback_safe: bool,
    pub blocked: String,
    pub image: String,
    pub release_url: String,
}

/// A run on the page.
#[derive(Debug, Clone)]
pub struct RunView {
    pub id: Uuid,
    pub kind: String,
    pub from_version: String,
    pub to_version: String,
    pub status: String,
    pub step: String,
    pub message: String,
    pub requested_by: String,
    pub node: String,
    pub backup: String,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub log: Vec<LogLine>,
    pub active: bool,
}

/// One progress entry.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub at: String,
    pub step: String,
    pub detail: String,
}

impl From<RunRow> for RunView {
    fn from(r: RunRow) -> Self {
        let log = r
            .log
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|e| LogLine {
                        at: e["at"]
                            .as_str()
                            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                            .map(|d| d.with_timezone(&Utc).format("%H:%M:%S").to_string())
                            .unwrap_or_default(),
                        step: e["step"].as_str().unwrap_or_default().to_owned(),
                        detail: e["detail"].as_str().unwrap_or_default().to_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            active: matches!(r.status.as_str(), "running" | "restarting"),
            id: r.id,
            kind: r.kind,
            from_version: r.from_version,
            to_version: r.to_version,
            status: r.status,
            step: r.step.unwrap_or_default(),
            message: r.message.unwrap_or_default(),
            requested_by: r.requested_by_email.unwrap_or_default(),
            node: r.node.unwrap_or_default(),
            backup: r.backup.unwrap_or_default(),
            created_at: r.created_at,
            finished_at: r.finished_at,
            log,
        }
    }
}

/// A problem of the last check.
#[derive(Debug, Clone)]
pub struct ProblemView {
    pub version: String,
    pub trust: String,
    pub reason: String,
}

/// A notice shown on the dashboard and the updates page.
#[derive(Debug, Clone)]
pub struct NoticeView {
    pub id: Uuid,
    pub text: String,
    pub link: String,
    pub tone: &'static str,
}

/// The update banner of `/platform` (and the updates page).
#[derive(Debug, Clone, Default)]
pub struct UpdateBanner {
    /// Newest installable version, if newer than the running one.
    pub available: Option<String>,
    /// Banner text for `available`.
    pub available_text: String,
    /// It fixes a security issue.
    pub security: bool,
    /// Signature verified.
    pub verified: bool,
    /// Unread notices.
    pub notices: Vec<NoticeView>,
}

impl UpdateBanner {
    /// Anything to show.
    pub fn visible(&self) -> bool {
        self.available.is_some() || !self.notices.is_empty()
    }
}

fn semver_desc(rows: &mut [hos_db::repo::updates::ReleaseRow]) {
    rows.sort_by(|a, b| {
        semver::Version::parse(&b.version)
            .ok()
            .cmp(&semver::Version::parse(&a.version).ok())
    });
}

fn newer_than_running(state: &AppState, version: &str) -> bool {
    match semver::Version::parse(version) {
        Ok(v) => v > state.updates.build.semver(),
        Err(_) => false,
    }
}

/// The newest installable release newer than the running build:
/// `(version, is a security fix)`. Never fails (`None` on any error).
pub async fn available_release(state: &AppState) -> Option<(String, bool)> {
    let mut rows = repo::list_releases(&state.pool).await.ok()?;
    semver_desc(&mut rows);
    rows.iter()
        .find(|r| r.blocked.is_none() && newer_than_running(state, &r.version))
        .map(|r| (r.version.clone(), r.severity == "security"))
}

/// Banner data for `admin` (newest release + unread notices). Never fails:
/// a missing table (older database) just shows nothing.
pub async fn banner(state: &AppState, admin: &PlatformAdmin, ctx: &PageCtx) -> UpdateBanner {
    let mut out = UpdateBanner::default();
    if let Ok(mut rows) = repo::list_releases(&state.pool).await {
        semver_desc(&mut rows);
        if let Some(r) = rows
            .iter()
            .find(|r| r.blocked.is_none() && newer_than_running(state, &r.version))
        {
            out.security = r.severity == "security";
            out.verified = r.trust == "verified";
            out.available_text = ctx.tv(
                if out.security {
                    "updates.banner.security"
                } else {
                    "updates.banner.available"
                },
                "version",
                &r.version,
            );
            out.available = Some(r.version.clone());
        }
    }
    if let Ok(rows) = repo::unread_notifications(&state.pool, admin.admin.id, 5).await {
        out.notices = rows
            .into_iter()
            // "available" is the banner itself; only outcomes are listed.
            .filter(|n| n.kind != "update.available")
            .map(|n| {
                let version = n.data["version"].as_str().unwrap_or_default().to_owned();
                let kind = n.kind.replace('.', "_");
                NoticeView {
                    id: n.id,
                    text: ctx.tv(&format!("updates.notice.{kind}"), "version", version),
                    tone: match kind.as_str() {
                        "update_succeeded" => "success",
                        "update_failed" | "update_rolled_back" => "error",
                        _ => "info",
                    },
                    link: n
                        .link_url
                        .filter(|l| l.starts_with("/platform/"))
                        .unwrap_or_else(|| BASE.to_owned()),
                }
            })
            .collect();
    }
    out
}

// ---------------------------------------------------------------------------
// overview
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "platform/updates.html")]
struct UpdatesPage {
    ctx: PageCtx,
    version: String,
    commit: String,
    build_date: String,
    target: String,
    profile: String,
    install: String,
    install_detail: String,
    container: bool,
    can_apply: bool,
    apply_blocker: String,
    source: String,
    configured: bool,
    source_error: String,
    keys_builtin: usize,
    keys_endorsed: usize,
    signing_ready: bool,
    channel: String,
    auto_check: bool,
    notify_email: String,
    webhook_url: String,
    webhook_method: String,
    webhook_secret_set: bool,
    last_check_at: Option<DateTime<Utc>>,
    last_check_error: String,
    next_check_at: Option<DateTime<Utc>>,
    problems: Vec<ProblemView>,
    releases: Vec<ReleaseView>,
    runs: Vec<RunView>,
    active_run: Option<RunView>,
    backup_plan: String,
    totp: bool,
    maintenance: bool,
    banner: UpdateBanner,
    channels: Vec<(String, String)>,
    methods: Vec<(String, String)>,
    compose_commands: String,
}
crate::page!(UpdatesPage);

impl UpdatesPage {
    fn channel_label(&self, c: &str) -> String {
        self.ctx.t(&format!("updates.channel.{c}"))
    }

    fn run_status(&self, s: &str) -> String {
        self.ctx.t(&format!("updates.run_status.{s}"))
    }

    fn run_tone(&self, s: &str) -> &'static str {
        run_tone(s)
    }

    fn short_commit(&self) -> &str {
        self.commit.get(..12).unwrap_or(&self.commit)
    }

    fn auto_hint(&self) -> String {
        self.ctx.t(if self.auto_check {
            "updates.settings.auto_on"
        } else {
            "updates.settings.auto_off"
        })
    }

    fn check_hint(&self) -> String {
        if self.last_check_error.is_empty() {
            String::new()
        } else {
            self.ctx.t("updates.check.failed")
        }
    }

    fn check_tone(&self) -> &'static str {
        if self.last_check_error.is_empty() {
            ""
        } else {
            "down"
        }
    }

    fn empty_text(&self) -> String {
        self.ctx.t(if self.configured {
            "updates.releases.none_text"
        } else {
            "updates.source.off_short"
        })
    }

    fn backup_text(&self) -> String {
        self.ctx.t(&format!("updates.backup.{}", self.backup_plan))
    }

    fn backup_label(&self) -> String {
        self.ctx
            .t(&format!("updates.backup.plan_{}", self.backup_plan))
    }

    fn kind_label(&self, k: &str) -> String {
        self.ctx.t(&format!("updates.kind.{k}"))
    }

    /// Element id for the copy button (`.` would break the CSS selector).
    fn compose_id(&self, version: &str) -> String {
        let v: String = version
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        format!("compose-{v}")
    }

    fn trust_label(&self, t: &str) -> String {
        self.ctx.t(&format!("updates.trust.{t}"))
    }

    fn secret_hint(&self) -> String {
        self.ctx.t(if self.webhook_secret_set {
            "updates.settings.secret_set"
        } else {
            "updates.settings.secret_unset"
        })
    }
}

fn run_tone(s: &str) -> &'static str {
    match s {
        "succeeded" => "success",
        "failed" | "rolled_back" => "danger",
        "requested" => "info",
        _ => "warning",
    }
}

fn step_label(ctx: &PageCtx, s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let key = format!("updates.step.{s}");
    let t = ctx.t(&key);
    if t == key { s.to_owned() } else { t }
}

fn install_texts(ctx: &PageCtx, kind: &InstallKind) -> (String, String) {
    match kind {
        InstallKind::Binary(b) => (
            ctx.t("updates.install.binary"),
            ctx.tv(
                if b.supervised {
                    "updates.install.binary_detail"
                } else {
                    "updates.install.binary_unsupervised"
                },
                "root",
                b.root.display(),
            ),
        ),
        InstallKind::Container(k) => (
            ctx.tv("updates.install.container", "kind", k.as_str()),
            ctx.t("updates.install.container_detail"),
        ),
        InstallKind::Unknown { reason } => (
            ctx.t("updates.install.unknown"),
            ctx.tv("updates.install.unknown_detail", "reason", reason),
        ),
    }
}

/// `GET /platform/updates`.
pub async fn page(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let title = ctx.t("updates.title");
    ctx.set_title(title.clone());
    ctx.panel = Some(menu::chrome(&ctx, vec![Crumb::current(title)]));
    let rt = &state.updates;
    let mut conn = state.pool.acquire().await?;
    let st = repo::state(&mut conn).await?;
    drop(conn);
    let mut rows = repo::list_releases(&state.pool).await?;
    semver_desc(&mut rows);
    let releases = rows
        .into_iter()
        .filter(|r| newer_than_running(&state, &r.version))
        .map(|r| {
            let verified = r.trust == "verified";
            ReleaseView {
                security: r.severity == "security",
                verified,
                trust_detail: r.trust_detail.unwrap_or_default(),
                published_at: r.published_at,
                notes_html: if verified {
                    crate::markdown::render(&r.notes_md)
                } else {
                    String::new()
                },
                notes_text: if verified {
                    String::new()
                } else {
                    r.notes_md.chars().take(20_000).collect()
                },
                migrations: r.migrations,
                rollback_safe: r.rollback_safe,
                blocked: r.blocked.unwrap_or_default(),
                image: r.container_image.unwrap_or_default(),
                release_url: r
                    .release_url
                    .filter(|u| u.starts_with("https://"))
                    .unwrap_or_default(),
                version: r.version,
            }
        })
        .collect::<Vec<_>>();
    let runs: Vec<RunView> = repo::list_runs(&state.pool, 20)
        .await?
        .into_iter()
        .map(RunView::from)
        .collect();
    let active_run = runs.iter().find(|r| r.active).cloned();
    let problems = st
        .last_check_problems
        .as_array()
        .map(|a| {
            a.iter()
                .map(|p| ProblemView {
                    version: p["version"].as_str().unwrap_or("?").to_owned(),
                    trust: p["trust"].as_str().unwrap_or_default().to_owned(),
                    reason: p["reason"].as_str().unwrap_or_default().to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    let source = rt.source();
    let (configured, source_text, source_error) = match &source {
        Ok(Some(s)) => (true, s.describe(), String::new()),
        Ok(None) => (false, String::new(), String::new()),
        Err(e) => (false, String::new(), e.clone()),
    };
    let keys_builtin = rt.builtin_keys().map(|r| r.len()).unwrap_or(0);
    let keys_endorsed = if keys_builtin > 0 {
        repo::endorsed_keys(&state.pool).await?.len()
    } else {
        0
    };
    let install = rt.install();
    let (install_label, install_detail) = install_texts(&ctx, &install);
    let apply_blocker = match &install {
        InstallKind::Binary(b) if b.supervised => {
            if keys_builtin == 0 {
                ctx.t("updates.apply.no_keys")
            } else {
                String::new()
            }
        }
        InstallKind::Binary(_) => ctx.t("updates.apply.no_supervisor"),
        InstallKind::Container(_) => String::new(),
        InstallKind::Unknown { .. } => ctx.t("updates.apply.unknown_install"),
    };
    let backup_plan = match &install {
        InstallKind::Binary(b) => BackupPlan::from_env(&b.layout().backups_dir()),
        _ => BackupPlan::from_env(std::path::Path::new("/nonexistent")),
    };
    let channels = ["stable", "beta"]
        .iter()
        .map(|c| ((*c).to_owned(), ctx.t(&format!("updates.channel.{c}"))))
        .collect();
    let methods = vec![
        ("GET".to_owned(), "GET".to_owned()),
        ("POST".to_owned(), "POST".to_owned()),
    ];
    let banner = banner(&state, &admin, &ctx).await;
    let build = rt.build.clone();
    render(&UpdatesPage {
        version: build.version.clone(),
        commit: build.commit.clone(),
        build_date: build
            .build_date
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        target: build.target.clone(),
        profile: build.profile.clone(),
        container: matches!(install, InstallKind::Container(_)),
        can_apply: install.can_apply_in_place() && keys_builtin > 0,
        install: install_label,
        install_detail,
        apply_blocker,
        source: source_text,
        configured,
        source_error,
        keys_builtin,
        keys_endorsed,
        signing_ready: keys_builtin > 0,
        channel: st.channel,
        auto_check: st.auto_check,
        notify_email: st.notify_email.unwrap_or_default(),
        webhook_url: st.webhook_url.unwrap_or_default(),
        webhook_method: st.webhook_method,
        webhook_secret_set: st.webhook_secret_enc.is_some(),
        last_check_at: st.last_check_at,
        last_check_error: st.last_check_error.unwrap_or_default(),
        next_check_at: st.next_check_at,
        problems,
        releases,
        runs,
        active_run,
        backup_plan: backup_plan.as_str().to_owned(),
        totp: admin
            .staff
            .as_ref()
            .map_or(admin.admin.totp_enabled, |u| u.totp_enabled),
        maintenance: st.maintenance,
        banner,
        channels,
        methods,
        compose_commands: "docker compose pull\ndocker compose up -d".to_owned(),
        ctx,
    })
}

// ---------------------------------------------------------------------------
// actions
// ---------------------------------------------------------------------------

fn error_text(ctx: &PageCtx, e: &UpdateError) -> String {
    ctx.tv2(
        "updates.errors.with_detail",
        "what",
        ctx.t(&format!("updates.error_kind.{}", e.kind())),
        "detail",
        e,
    )
}

/// `POST /platform/updates/check`.
pub async fn check(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    ctx: PageCtx,
) -> AppResult<Response> {
    check_rate(&state.limits.whois, &req.ip.key("platform-update-check"))?;
    let result = runtime::check_now(&state).await;
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.update.checked",
        None,
        json!({ "ok": result.is_ok() }),
    )
    .await;
    let flash = match result {
        Ok(s) => match s.newest {
            Some(v) => SetFlash::success(ctx.tv("updates.check.found", "version", v)),
            None => SetFlash::info(ctx.t("updates.check.none")),
        },
        Err(e) => SetFlash::error(error_text(&ctx, &e)),
    };
    Ok(flash::redirect(BASE, flash))
}

#[derive(Debug, Default, Deserialize)]
pub struct SettingsForm {
    #[serde(default)]
    channel: String,
    #[serde(default)]
    auto_check: Option<String>,
    #[serde(default)]
    notify_email: String,
    #[serde(default)]
    webhook_url: String,
    #[serde(default)]
    webhook_method: String,
    #[serde(default)]
    webhook_secret: String,
    #[serde(default)]
    clear_secret: Option<String>,
    /// Re-authentication, needed when the channel or the deploy webhook
    /// (URL, method, secret) changes.
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: String,
}

/// `host:port` of a webhook URL (lower-case; `None` when not a URL).
fn webhook_origin(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    Some(format!(
        "{}:{}",
        u.host_str()?.to_ascii_lowercase(),
        u.port_or_known_default().unwrap_or(0)
    ))
}

/// `POST /platform/updates/settings`.
pub async fn save_settings(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    ctx: PageCtx,
    Form(form): Form<SettingsForm>,
) -> AppResult<Response> {
    let channel = Channel::parse(&form.channel).unwrap_or_default();
    let email = forms::normalize_email(&form.notify_email);
    if !email.is_empty() && !forms::valid_email(&email) {
        return Ok(flash::redirect(
            BASE,
            SetFlash::error(ctx.t("auth.errors.email_invalid")),
        ));
    }
    let url = form.webhook_url.trim().to_owned();
    if !url.is_empty() {
        let fetcher = state
            .updates
            .webhook_fetcher()
            .map_err(AppError::internal)?;
        if let Err(e) = hos_update::webhook::validate_url(&fetcher, &url) {
            return Ok(flash::redirect(BASE, SetFlash::error(error_text(&ctx, &e))));
        }
    }
    let method = hos_update::webhook::WebhookMethod::parse(&form.webhook_method)
        .unwrap_or_default()
        .as_str()
        .to_owned();
    let secret = form.webhook_secret.trim();
    let clear = forms::checked(form.clear_secret.as_deref());
    // A stolen console session must not redirect the deploy webhook (and
    // its secret) or switch the channel: such changes need the password
    // (+ 2FA code) again.
    let current = {
        let mut conn = state.pool.acquire().await?;
        repo::state(&mut conn).await?
    };
    let new_url = (!url.is_empty()).then(|| url.clone());
    let sensitive = channel.as_str() != current.channel
        || new_url != current.webhook_url
        || method != current.webhook_method
        || !secret.is_empty()
        || clear;
    if sensitive {
        let confirm = ConfirmForm {
            password: form.password.clone(),
            code: form.code.clone(),
            ..ConfirmForm::default()
        };
        if !reauthenticate(&state, &admin, &req, &confirm).await? {
            audit_action(
                &state,
                &admin,
                &req,
                "platform.update.reauth_failed",
                json!({ "settings": true }),
            )
            .await;
            return Ok(flash::redirect(
                BASE,
                SetFlash::error(ctx.t("updates.settings.reauth_required")),
            ));
        }
    }
    // The stored secret only ever goes to the host it was set for.
    let host_changed = current.webhook_url.as_deref().and_then(webhook_origin)
        != new_url.as_deref().and_then(webhook_origin);
    let webhook_secret_enc = if clear || (secret.is_empty() && host_changed) {
        Some(None)
    } else if secret.is_empty() {
        None
    } else {
        Some(Some(
            state
                .secret_box
                .encrypt(secret.as_bytes(), runtime::WEBHOOK_SECRET_AAD)
                .map_err(AppError::internal)?,
        ))
    };
    let input = SettingsInput {
        channel: channel.as_str().to_owned(),
        auto_check: forms::checked(form.auto_check.as_deref()),
        notify_email: (!email.is_empty()).then_some(email),
        webhook_url: (!url.is_empty()).then_some(url),
        webhook_method: method,
        webhook_secret_enc,
    };
    repo::save_settings(&state.pool, &input).await?;
    audit_platform(
        &state,
        &admin,
        &req,
        None,
        "platform.update.settings",
        None,
        json!({
            "channel": input.channel,
            "auto_check": input.auto_check,
            "notify_email": input.notify_email,
            "webhook": input.webhook_url.is_some(),
            "webhook_secret_changed": input.webhook_secret_enc.is_some(),
        }),
    )
    .await;
    Ok(flash::redirect(
        BASE,
        SetFlash::success(ctx.t("platform.common.saved")),
    ))
}

#[derive(Debug, Default, Deserialize)]
pub struct ConfirmForm {
    #[serde(default)]
    version: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    backup_confirmed: Option<String>,
}

/// Re-authentication for installs / deploys: the password, plus a TOTP
/// (or recovery) code when 2FA is on.
async fn reauthenticate(
    state: &AppState,
    admin: &PlatformAdmin,
    req: &AuditCtx,
    form: &ConfirmForm,
) -> AppResult<bool> {
    check_rate(
        &state.limits.two_factor,
        &req.ip.key("platform-update-reauth"),
    )?;
    // Single-company mode: the owner confirms with their own password and
    // two-factor code (the operator record has neither).
    let stored = match &admin.staff {
        Some(user) => user.password_hash.clone(),
        None => admin.admin.password_hash.clone(),
    };
    let pw = form.password.clone();
    let password_ok = tokio::task::spawn_blocking(move || {
        hos_auth::password::verify_user_password(&pw, stored.as_deref())
    })
    .await?;
    if !password_ok {
        return Ok(false);
    }
    if let Some(user) = &admin.staff {
        if !user.totp_enabled {
            return Ok(true);
        }
        return crate::routes::auth::two_factor::verify_staff_code(
            state,
            user.tenant_id,
            user.id,
            &form.code,
        )
        .await;
    }
    if admin.admin.totp_enabled {
        return super::login::check_admin_code(state, admin.admin.id, &form.code).await;
    }
    Ok(true)
}

async fn audit_action(
    state: &AppState,
    admin: &PlatformAdmin,
    req: &AuditCtx,
    action: &str,
    changes: Value,
) {
    audit_platform(state, admin, req, None, action, None, changes).await;
}

/// `POST /platform/updates/apply` — in-place install (bare binary).
pub async fn apply(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    ctx: PageCtx,
    Form(form): Form<ConfirmForm>,
) -> AppResult<Response> {
    let version = form.version.trim().to_owned();
    if !reauthenticate(&state, &admin, &req, &form).await? {
        audit_action(
            &state,
            &admin,
            &req,
            "platform.update.reauth_failed",
            json!({ "version": version }),
        )
        .await;
        return Ok(flash::redirect(
            BASE,
            SetFlash::error(ctx.t("updates.apply.reauth_failed")),
        ));
    }
    let confirmed = forms::checked(form.backup_confirmed.as_deref());
    let who = runtime::Requester::of(&admin);
    let started = match runtime::prepare_apply(
        &state,
        &version,
        confirmed,
        runtime::ApplyMode::Service,
    )
    .await
    {
        Ok(p) => runtime::start_apply(&state, p, &who, runtime::ApplyMode::Service).await,
        Err(e) => Err(e),
    };
    audit_action(
        &state,
        &admin,
        &req,
        "platform.update.apply",
        json!({
            "version": version,
            "from": state.updates.build.version,
            "backup_confirmed": confirmed,
            "ok": started.is_ok(),
            "error": started.as_ref().err().map(UpdateError::kind),
        }),
    )
    .await;
    match started {
        Ok(run) => Ok(flash::redirect(
            &format!("{BASE}/runs/{run}"),
            SetFlash::info(ctx.t("updates.apply.started")),
        )),
        Err(e) => Ok(flash::redirect(BASE, SetFlash::error(error_text(&ctx, &e)))),
    }
}

/// `POST /platform/updates/deploy` — deploy webhook (containers).
pub async fn deploy(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    ctx: PageCtx,
    Form(form): Form<ConfirmForm>,
) -> AppResult<Response> {
    let version = form.version.trim().to_owned();
    if !reauthenticate(&state, &admin, &req, &form).await? {
        audit_action(
            &state,
            &admin,
            &req,
            "platform.update.reauth_failed",
            json!({ "version": version }),
        )
        .await;
        return Ok(flash::redirect(
            BASE,
            SetFlash::error(ctx.t("updates.apply.reauth_failed")),
        ));
    }
    let who = runtime::Requester::of(&admin);
    let result = runtime::request_deploy(&state, &version, &who).await;
    audit_action(
        &state,
        &admin,
        &req,
        "platform.update.deploy_webhook",
        json!({
            "version": version,
            "ok": result.is_ok(),
            "error": result.as_ref().err().map(UpdateError::kind),
        }),
    )
    .await;
    match result {
        Ok(run) => Ok(flash::redirect(
            &format!("{BASE}/runs/{run}"),
            SetFlash::success(ctx.t("updates.deploy.requested")),
        )),
        Err(e) => Ok(flash::redirect(BASE, SetFlash::error(error_text(&ctx, &e)))),
    }
}

/// `POST /platform/updates/maintenance/off` — ends maintenance mode left on
/// by an interrupted update (refused while a run is active).
pub async fn maintenance_off(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    req: AuditCtx,
    ctx: PageCtx,
    Form(form): Form<ConfirmForm>,
) -> AppResult<Response> {
    if !reauthenticate(&state, &admin, &req, &form).await? {
        return Ok(flash::redirect(
            BASE,
            SetFlash::error(ctx.t("updates.apply.reauth_failed")),
        ));
    }
    if repo::active_run(&state.pool).await?.is_some() {
        return Ok(flash::redirect(
            BASE,
            SetFlash::error(ctx.t("updates.maintenance.active_run")),
        ));
    }
    repo::set_maintenance(&state.pool, false, None).await?;
    state.updates.set_maintenance_cache(false);
    audit_action(
        &state,
        &admin,
        &req,
        "platform.update.maintenance_off",
        json!({}),
    )
    .await;
    Ok(flash::redirect(
        BASE,
        SetFlash::success(ctx.t("updates.maintenance.ended")),
    ))
}

#[derive(Debug, Default, Deserialize)]
pub struct DismissForm {
    #[serde(default)]
    next: String,
}

/// `POST /platform/updates/notifications/{id}/dismiss`.
pub async fn dismiss(
    State(state): State<AppState>,
    admin: PlatformAdmin,
    Path(id): Path<String>,
    Form(form): Form<DismissForm>,
) -> AppResult<Response> {
    let id: Uuid = id.parse().map_err(|_| AppError::NotFound)?;
    repo::mark_notification_read(&state.pool, id, admin.admin.id).await?;
    let next = if form.next == "/platform" {
        "/platform"
    } else {
        BASE
    };
    Ok(Redirect::to(next).into_response())
}

// ---------------------------------------------------------------------------
// run progress
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "platform/update_run.html")]
struct RunPage {
    ctx: PageCtx,
    run: RunView,
}
crate::page!(RunPage);

#[derive(Template)]
#[template(path = "platform/_update_run_status.html")]
struct RunStatus {
    ctx: PageCtx,
    run: RunView,
}
crate::page!(RunStatus);

macro_rules! run_helpers {
    ($t:ty) => {
        impl $t {
            fn run_status(&self, s: &str) -> String {
                self.ctx.t(&format!("updates.run_status.{s}"))
            }

            fn run_tone(&self, s: &str) -> &'static str {
                run_tone(s)
            }

            fn step_label(&self, s: &str) -> String {
                step_label(&self.ctx, s)
            }
        }
    };
}
run_helpers!(RunPage);
run_helpers!(RunStatus);

impl RunPage {
    fn kind_label(&self, k: &str) -> String {
        self.ctx.t(&format!("updates.kind.{k}"))
    }
}

async fn load_run(state: &AppState, id: &str) -> AppResult<RunView> {
    let id: Uuid = id.parse().map_err(|_| AppError::NotFound)?;
    let run = repo::get_run(&state.pool, id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(RunView::from(run))
}

/// `GET /platform/updates/runs/{id}`.
pub async fn run_page(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    Path(id): Path<String>,
    mut ctx: PageCtx,
) -> AppResult<Response> {
    let run = load_run(&state, &id).await?;
    let title = ctx.tv2(
        "updates.run.title",
        "from",
        &run.from_version,
        "to",
        &run.to_version,
    );
    ctx.set_title(title.clone());
    let root = ctx.t("updates.title");
    ctx.panel = Some(menu::chrome(
        &ctx,
        vec![Crumb::link(root, BASE), Crumb::current(title)],
    ));
    render(&RunPage { ctx, run })
}

/// `GET /platform/updates/runs/{id}/status` — htmx fragment polled every
/// two seconds; served by whichever binary is running, so it reports the
/// outcome after the restart too.
pub async fn run_status(
    State(state): State<AppState>,
    _admin: PlatformAdmin,
    Path(id): Path<String>,
    _htmx: Htmx,
    ctx: PageCtx,
) -> AppResult<Response> {
    let run = load_run(&state, &id).await?;
    let mut res = render(&RunStatus { ctx, run })?;
    res.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    Ok(res)
}
