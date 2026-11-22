-- HostForge schema, part 3: identity (users, OAuth identities, sessions, RBAC,
-- 2FA, one-time tokens, API keys, audit log).

-- Tenant-scoped login accounts: customers (`client`) and hosting company staff (`staff`).
create table users (
    id                  uuid primary key default hf_uuid_v7(),
    tenant_id           uuid not null references tenants (id) on delete cascade,
    email               citext not null check (position('@' in email) > 1),
    -- argon2id PHC string; null for OAuth-only accounts.
    password_hash       text,
    kind                text not null check (kind in ('client', 'staff')),
    first_name          text not null default '',
    last_name           text not null default '',
    phone               text,
    locale              text not null default 'tr' check (locale in ('tr', 'en')),
    timezone            text,
    avatar_url          text,
    email_verified_at   timestamptz,
    totp_enabled        boolean not null default false,
    status              text not null default 'active' check (status in ('active', 'locked', 'disabled')),
    failed_login_count  integer not null default 0 check (failed_login_count >= 0),
    locked_until        timestamptz,
    last_login_at       timestamptz,
    last_login_ip       inet,
    password_changed_at timestamptz,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    unique (tenant_id, email)
);
create index users_tenant_kind_idx on users (tenant_id, kind, created_at desc);

-- Linked social logins (Google / GitHub).
create table user_identities (
    id                uuid primary key default hf_uuid_v7(),
    tenant_id         uuid not null references tenants (id) on delete cascade,
    user_id           uuid not null references users (id) on delete cascade,
    provider          text not null check (provider in ('google', 'github')),
    provider_user_id  text not null,
    email             citext,
    display_name      text,
    avatar_url        text,
    raw_profile       jsonb not null default '{}'::jsonb,
    last_used_at      timestamptz,
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now(),
    unique (tenant_id, provider, provider_user_id),
    unique (user_id, provider)
);
create index user_identities_tenant_user_idx on user_identities (tenant_id, user_id);

-- Login sessions of tenant users or platform admins. Only the SHA-256 of the
-- cookie token is stored (`hf_core::crypto::hash_token`).
create table sessions (
    id                 uuid primary key default hf_uuid_v7(),
    -- Null only for platform admin sessions.
    tenant_id          uuid references tenants (id) on delete cascade,
    token_hash         text not null unique,
    user_id            uuid references users (id) on delete cascade,
    platform_admin_id  uuid references platform_admins (id) on delete cascade,
    -- Staff user (kind = 'staff') or platform admin who is impersonating `user_id`.
    impersonator_id    uuid,
    impersonator_kind  text check (impersonator_kind in ('staff', 'platform_admin')),
    csrf_token         text not null default '',
    -- When the second factor was passed (null: password-only session / 2FA pending).
    mfa_verified_at    timestamptz,
    ip                 inet,
    user_agent         text,
    data               jsonb not null default '{}'::jsonb,
    expires_at         timestamptz not null,
    last_seen_at       timestamptz not null default now(),
    created_at         timestamptz not null default now(),
    check (num_nonnulls(user_id, platform_admin_id) = 1),
    check (user_id is null or tenant_id is not null),
    check ((impersonator_id is null) = (impersonator_kind is null))
);
create index sessions_tenant_user_idx on sessions (tenant_id, user_id);
create index sessions_platform_admin_idx on sessions (platform_admin_id) where platform_admin_id is not null;
create index sessions_expires_idx on sessions (expires_at);

create table roles (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    slug         text not null check (slug ~ '^[a-z0-9_-]+$'),
    name         text not null,
    description  text,
    -- Built-in roles cannot be deleted from the admin UI.
    is_system    boolean not null default false,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    unique (tenant_id, slug)
);

-- Permissions are `area.action` strings (`billing.manage`), `area.*` or `*`.
create table role_permissions (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    role_id     uuid not null references roles (id) on delete cascade,
    permission  text not null check (permission ~ '^(\*|[a-z][a-z0-9_]*\.(\*|[a-z][a-z0-9_]*))$'),
    created_at  timestamptz not null default now(),
    unique (role_id, permission)
);
create index role_permissions_tenant_role_idx on role_permissions (tenant_id, role_id);

create table user_roles (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    user_id     uuid not null references users (id) on delete cascade,
    role_id     uuid not null references roles (id) on delete cascade,
    created_at  timestamptz not null default now(),
    unique (user_id, role_id)
);
create index user_roles_tenant_user_idx on user_roles (tenant_id, user_id);
create index user_roles_role_idx on user_roles (role_id);

-- TOTP secret (encrypted with hf_core::crypto::SecretBox) and hashed recovery codes.
create table totp_secrets (
    id                  uuid primary key default hf_uuid_v7(),
    tenant_id           uuid not null references tenants (id) on delete cascade,
    user_id             uuid not null unique references users (id) on delete cascade,
    secret_enc          bytea not null,
    recovery_code_hashes text[] not null default '{}',
    confirmed_at        timestamptz,
    last_used_step      bigint,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now()
);
create index totp_secrets_tenant_idx on totp_secrets (tenant_id);

create table password_resets (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    user_id     uuid not null references users (id) on delete cascade,
    token_hash  text not null unique,
    expires_at  timestamptz not null,
    used_at     timestamptz,
    ip          inet,
    created_at  timestamptz not null default now()
);
create index password_resets_tenant_user_idx on password_resets (tenant_id, user_id);

create table email_verifications (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    user_id      uuid not null references users (id) on delete cascade,
    email        citext not null,
    token_hash   text not null unique,
    expires_at   timestamptz not null,
    verified_at  timestamptz,
    created_at   timestamptz not null default now()
);
create index email_verifications_tenant_user_idx on email_verifications (tenant_id, user_id);

-- REST API keys (`/api/v1`). Only the hash is stored; `prefix` is shown in the UI.
create table api_keys (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    user_id       uuid references users (id) on delete cascade,
    name          text not null,
    prefix        text not null,
    key_hash      text not null unique,
    -- Permission strings like role_permissions; empty = inherit the user's permissions.
    scopes        text[] not null default '{}',
    allowed_ips   inet[] not null default '{}',
    last_used_at  timestamptz,
    last_used_ip  inet,
    expires_at    timestamptz,
    revoked_at    timestamptz,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now()
);
create index api_keys_tenant_idx on api_keys (tenant_id, created_at desc);

-- Append-only audit trail of admin / platform / system actions.
create table audit_log (
    id               uuid primary key default hf_uuid_v7(),
    -- Null for platform-level actions.
    tenant_id        uuid references tenants (id) on delete cascade,
    actor_kind       text not null check (actor_kind in ('user', 'platform_admin', 'system', 'api_key')),
    actor_id         uuid,
    impersonator_id  uuid,
    action           text not null,
    entity_type      text,
    entity_id        uuid,
    ip               inet,
    user_agent       text,
    -- {"before": {...}, "after": {...}} or a field diff.
    changes          jsonb not null default '{}'::jsonb,
    metadata         jsonb not null default '{}'::jsonb,
    created_at       timestamptz not null default now()
);
create index audit_log_tenant_created_idx on audit_log (tenant_id, created_at desc);
create index audit_log_tenant_entity_idx on audit_log (tenant_id, entity_type, entity_id, created_at desc);
create index audit_log_tenant_actor_idx on audit_log (tenant_id, actor_id, created_at desc);

select hf_attach_updated_at_triggers();
