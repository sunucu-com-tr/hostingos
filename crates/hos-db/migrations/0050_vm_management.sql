-- HostingOS schema, part 50: VPS management (hos-billing `vm_ops`,
-- hos-web console relay). Additive only.
--
--   * vm_console_sessions — one row per console session: the one-time token
--                           (SHA-256 only), who / which service / tenant,
--                           connect + end times, traffic and close reason.
--                           Doubles as the console audit trail.
--   * vm_operations       — VM actions with progress and failure reporting
--                           (power, reinstall, snapshots, backups, ISO,
--                           rescue, firewall, rDNS, password reset). Never
--                           holds secrets: passwords are shown once and not
--                           stored here.
--   * client_ssh_keys     — public SSH keys a client keeps in its profile
--                           (offered at order, reinstall and rescue).
--   * vm_iso_images       — ISO images staff approved per virtualization
--                           integration (allow-list, like OS templates).

create table vm_console_sessions (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    service_id    uuid not null references services (id) on delete cascade,
    user_id       uuid not null references users (id) on delete cascade,
    is_staff      boolean not null default false,
    token_hash    bytea not null unique,
    ip            inet,
    user_agent    text,
    issued_at     timestamptz not null default now(),
    expires_at    timestamptz not null,
    connected_at  timestamptz,
    ended_at      timestamptz,
    -- browser → VM / VM → browser, bytes
    bytes_in      bigint not null default 0,
    bytes_out     bigint not null default 0,
    close_reason  text,
    check (expires_at > issued_at)
);
create index vm_console_sessions_tenant_service_idx
    on vm_console_sessions (tenant_id, service_id, issued_at desc);
create index vm_console_sessions_open_idx
    on vm_console_sessions (tenant_id, user_id)
    where connected_at is not null and ended_at is null;

create table vm_operations (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    service_id   uuid not null references services (id) on delete cascade,
    kind         text not null check (kind in (
                     'power', 'reinstall', 'snapshot_create', 'snapshot_rollback',
                     'snapshot_delete', 'backup_restore', 'iso_mount', 'iso_unmount',
                     'rescue', 'firewall', 'reverse_dns', 'password_reset')),
    status       text not null default 'queued'
                     check (status in ('queued', 'running', 'succeeded', 'failed')),
    progress     smallint check (progress between 0 and 100),
    -- Short human summary ("reboot", snapshot name, ISO name …).
    detail       text not null default '',
    -- Provider task (`TaskRef`) while it runs.
    task         jsonb,
    error        text,
    actor_kind   text check (actor_kind in ('user', 'platform_admin', 'system', 'api_key')),
    actor_id     uuid,
    created_at   timestamptz not null default now(),
    finished_at  timestamptz
);
create index vm_operations_tenant_service_idx
    on vm_operations (tenant_id, service_id, created_at desc);
create index vm_operations_open_idx
    on vm_operations (tenant_id, service_id)
    where status in ('queued', 'running');

create table client_ssh_keys (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    client_id    uuid not null references clients (id) on delete cascade,
    name         text not null check (length(name) between 1 and 64),
    public_key   text not null check (length(public_key) between 16 and 16384),
    -- OpenSSH `SHA256:<base64>` fingerprint.
    fingerprint  text not null,
    created_by   uuid references users (id) on delete set null,
    created_at   timestamptz not null default now(),
    unique (tenant_id, client_id, fingerprint)
);
create index client_ssh_keys_tenant_client_idx on client_ssh_keys (tenant_id, client_id);

create table vm_iso_images (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    integration_id  uuid not null references integrations (id) on delete cascade,
    -- Provider id: Proxmox volid (`local:iso/x.iso`), Hetzner ISO name or id.
    iso_ref         text not null check (length(iso_ref) between 1 and 255),
    name            text not null check (length(name) between 1 and 128),
    is_active       boolean not null default true,
    created_at      timestamptz not null default now(),
    unique (tenant_id, integration_id, iso_ref)
);
create index vm_iso_images_tenant_integration_idx on vm_iso_images (tenant_id, integration_id);
