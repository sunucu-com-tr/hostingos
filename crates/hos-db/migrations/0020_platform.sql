-- HostForge schema, part 20: platform console and REST API support.
--
--   * platform_settings        — key/value settings of the SaaS operator
--                                (`signup.enabled`, `signup.trial_days`,
--                                `signup.default_plan`, ...). Not tenant scoped.
--   * platform_admins.totp_*   — replay protection + recovery codes of the
--                                platform operators' TOTP second factor.
--   * platform_signups         — self-service tenant sign-ups waiting for the
--                                e-mail verification (then provisioned).
--   * platform_handoffs        — one-time "sign in as tenant owner" tokens
--                                (platform host → tenant host hand-off).
--   * api_idempotency_keys     — `Idempotency-Key` replays of REST API POSTs.
--   * tenant_domains.last_*    — bookkeeping of the custom-domain DNS check.

create table platform_settings (
    key         text primary key check (key ~ '^[a-z0-9_]+(\.[a-z0-9_]+)*$'),
    value       jsonb not null,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now()
);

alter table platform_admins
    add column totp_last_step        bigint,
    add column totp_recovery_hashes  text[] not null default '{}';

create table platform_signups (
    id             uuid primary key default hf_uuid_v7(),
    email          citext not null,
    company_name   text not null check (length(btrim(company_name)) > 0),
    owner_name     text not null default '',
    slug           text not null
                   check (slug ~ '^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$'),
    plan_id        uuid references platform_plans (id) on delete set null,
    password_hash  text,
    locale         text not null default 'tr' check (locale in ('tr', 'en')),
    -- SHA-256 of the e-mail verification token.
    token_hash     text not null unique,
    ip             inet,
    expires_at     timestamptz not null,
    verified_at    timestamptz,
    tenant_id      uuid references tenants (id) on delete set null,
    created_at     timestamptz not null default now(),
    updated_at     timestamptz not null default now()
);
create index platform_signups_slug_idx on platform_signups (slug) where verified_at is null;
create index platform_signups_email_idx on platform_signups (email, created_at desc);

create table platform_handoffs (
    id          uuid primary key default hf_uuid_v7(),
    -- SHA-256 of the one-time token carried in the hand-off URL.
    token_hash  text not null unique,
    admin_id    uuid not null references platform_admins (id) on delete cascade,
    tenant_id   uuid not null references tenants (id) on delete cascade,
    user_id     uuid not null,
    expires_at  timestamptz not null,
    used_at     timestamptz,
    created_at  timestamptz not null default now(),
    constraint platform_handoffs_user_fk foreign key (tenant_id, user_id)
        references users (tenant_id, id) on delete cascade
);
create index platform_handoffs_expires_idx on platform_handoffs (expires_at);

create table api_idempotency_keys (
    id               uuid primary key default hf_uuid_v7(),
    tenant_id        uuid not null references tenants (id) on delete cascade,
    -- `key:<api_keys.id>` or `user:<users.id>` (session callers).
    principal        text not null,
    idempotency_key  text not null check (length(idempotency_key) between 1 and 255),
    method           text not null,
    path             text not null,
    -- SHA-256 of method + path + body: a reused key with another request is a 422.
    request_hash     text not null,
    status           text not null default 'processing' check (status in ('processing', 'completed')),
    response_status  integer,
    response_body    jsonb,
    expires_at       timestamptz not null default now() + interval '24 hours',
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now(),
    unique (tenant_id, principal, idempotency_key)
);
create index api_idempotency_keys_expires_idx on api_idempotency_keys (expires_at);

alter table tenant_domains
    add column last_checked_at  timestamptz,
    add column check_error      text;

select hf_attach_updated_at_triggers();
