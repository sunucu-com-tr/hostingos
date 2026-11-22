-- HostForge schema, part 22: admin settings, staff and developer screens.
--
--   * staff_invites   — e-mailed staff invitations (purpose-bound token hash,
--                       role to grant, 7-day expiry). The user row is created
--                       when the invitation is accepted (`/auth/invite`).
--   * tenant_assets   — uploaded logo / dark logo / favicon of a tenant
--                       (validated raster images, served by `/brand/asset/*`
--                       with cache headers). SVG is rejected on upload.

create table staff_invites (
    id                uuid primary key default hf_uuid_v7(),
    tenant_id         uuid not null references tenants (id) on delete cascade,
    email             citext not null,
    first_name        text not null default '',
    last_name         text not null default '',
    role_id           uuid not null references roles (id) on delete cascade,
    -- hf_auth::tokens::hash_one_time_token(Invite, token)
    token_hash        text not null unique,
    invited_by        uuid references users (id) on delete set null,
    locale            text not null default 'tr' check (locale in ('tr', 'en')),
    expires_at        timestamptz not null,
    accepted_at       timestamptz,
    accepted_user_id  uuid references users (id) on delete set null,
    revoked_at        timestamptz,
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now()
);
create index staff_invites_tenant_idx on staff_invites (tenant_id, created_at desc);
create index staff_invites_open_idx on staff_invites (tenant_id, email)
    where accepted_at is null and revoked_at is null;

create table tenant_assets (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    kind          text not null check (kind in ('logo', 'logo_dark', 'favicon')),
    content_type  text not null check (content_type in ('image/png', 'image/jpeg', 'image/webp', 'image/gif', 'image/x-icon')),
    data          bytea not null,
    sha256        text not null,
    size_bytes    integer not null check (size_bytes > 0 and size_bytes <= 1048576),
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    unique (tenant_id, kind)
);

select hf_attach_updated_at_triggers();
