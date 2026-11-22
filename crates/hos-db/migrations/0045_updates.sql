-- HostingOS schema, part 45: self-update (crate hos-update, /platform/updates,
-- docs/UPDATES.md). Platform-level tables, no tenant_id. Additive only.
--
--   * update_state          — singleton (id = true): settings (channel,
--                             automatic checks, notification e-mail, deploy
--                             webhook with an encrypted secret), the last
--                             check and the cluster-wide maintenance flag.
--   * update_releases       — newer versions found by the last checks, with
--                             the exact signed manifest + signature, so an
--                             install re-verifies them instead of trusting
--                             this table.
--   * update_runs           — every install / deploy-webhook / rollback:
--                             who, when, from → to, progress log, result. At
--                             most one run is active (partial unique index),
--                             on top of the advisory lock taken while applying.
--   * update_trusted_keys   — signing keys announced by verified manifests
--                             (key rotation); compiled-in keys are not stored.
--   * platform_notifications (+ _reads) — in-app notices for platform admins
--                             (update available, update finished / failed).

create table update_state (
    id                  boolean primary key default true check (id),
    channel             text not null default 'stable' check (channel in ('stable', 'beta')),
    auto_check          boolean not null default true,
    notify_email        citext,
    webhook_url         text,
    webhook_method      text not null default 'GET' check (webhook_method in ('GET', 'POST')),
    webhook_secret_enc  bytea,
    last_check_at       timestamptz,
    last_check_error    text,
    -- Signature / replay problems of the last check: [{version, trust, reason}].
    last_check_problems jsonb not null default '[]'::jsonb,
    next_check_at       timestamptz,
    -- Newest version platform admins were notified about.
    notified_version    text,
    maintenance         boolean not null default false,
    maintenance_reason  text,
    maintenance_since   timestamptz,
    updated_at          timestamptz not null default now()
);
insert into update_state (id) values (true) on conflict do nothing;

create table update_releases (
    version             text primary key check (length(version) between 5 and 64),
    channel             text not null check (channel in ('stable', 'beta')),
    trust               text not null check (trust in ('verified', 'unverified')),
    -- Key id (verified) or why it is not verifiable.
    trust_detail        text,
    severity            text not null default 'regular' check (severity in ('regular', 'security')),
    published_at        timestamptz not null,
    min_upgradable_from text,
    migrations          boolean not null default false,
    rollback_safe       boolean not null default false,
    -- Why it cannot be installed directly (min_upgradable_from …).
    blocked             text,
    notes_md            text not null default '',
    container_image     text,
    release_url         text,
    manifest            text not null,
    signature           text,
    first_seen_at       timestamptz not null default now(),
    last_seen_at        timestamptz not null default now()
);

create table update_runs (
    id                  uuid primary key default hf_uuid_v7(),
    kind                text not null check (kind in ('binary', 'webhook', 'rollback')),
    from_version        text not null,
    to_version          text not null,
    status              text not null default 'running'
                        check (status in ('running', 'restarting', 'requested', 'succeeded', 'failed', 'rolled_back')),
    step                text,
    message             text,
    -- [{at, step, detail}]
    log                 jsonb not null default '[]'::jsonb,
    requested_by        uuid references platform_admins (id) on delete set null,
    requested_by_email  text,
    node                text,
    backup              text,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    finished_at         timestamptz
);
create index update_runs_created_idx on update_runs (created_at desc);
create unique index update_runs_one_active_idx on update_runs ((true))
    where status in ('running', 'restarting');

create table update_trusted_keys (
    key_id          text primary key check (key_id ~ '^[0-9A-F]{16}$'),
    public_key      text not null,
    endorsed_by     text not null,
    endorsed_in     text not null,
    revoked_at      timestamptz,
    created_at      timestamptz not null default now()
);

create table platform_notifications (
    id          uuid primary key default hf_uuid_v7(),
    kind        text not null check (kind ~ '^[a-z0-9_.]+$'),
    data        jsonb not null default '{}'::jsonb,
    link_url    text,
    dedupe_key  text unique,
    created_at  timestamptz not null default now()
);
create index platform_notifications_created_idx on platform_notifications (created_at desc);

create table platform_notification_reads (
    notification_id    uuid not null references platform_notifications (id) on delete cascade,
    platform_admin_id  uuid not null references platform_admins (id) on delete cascade,
    read_at            timestamptz not null default now(),
    primary key (notification_id, platform_admin_id)
);
