-- HostingOS schema, part 29: data import (WHMCS → HostingOS, crate hos-import).
--
--   * import_runs          — one import session of a tenant: source connection
--                            (non-secret settings in `source_config`; password /
--                            API secret / cc_encryption_hash SecretBox-sealed in
--                            `secrets_enc`, AAD = hos_import::secrets_aad, wiped
--                            when the run ends), wizard mappings, per-entity
--                            progress with resume cursors, dry-run preview and
--                            import summary. The run row doubles as the
--                            background work item: the import runner claims
--                            `queued` / `rollback_queued` rows (and `running` /
--                            `rolling_back` rows whose heartbeat is stale) with
--                            FOR UPDATE SKIP LOCKED. At most one active run per
--                            tenant.
--   * import_id_map        — source record → HostingOS row, per tenant and
--                            source installation (`source_key`). Makes re-runs
--                            idempotent (update instead of insert) and lets a
--                            rollback delete exactly the rows a run created
--                            (`created_run_id`; adopted pre-existing rows have
--                            none). No FK on `target_id`: it points into many
--                            tables; a dangling entry is re-created on the
--                            next run.
--   * import_run_messages  — warnings / skips / conflicts / errors of a run
--                            (capped by the engine), the downloadable report.
--
-- Additive only; composite tenant FKs as in migration 0012.

create table import_runs (
    id                   uuid primary key default hf_uuid_v7(),
    tenant_id            uuid not null references tenants (id) on delete cascade,
    source_kind          text not null check (source_kind in ('whmcs_db', 'whmcs_api')),
    -- Namespace of the id map: the normalised WHMCS system URL (or host).
    source_key           text not null check (length(source_key) between 1 and 300),
    source_label         text not null default '',
    -- Host / port / database / user / TLS or API URL / identifier. Never secrets.
    source_config        jsonb not null default '{}'::jsonb check (jsonb_typeof(source_config) = 'object'),
    secrets_enc          bytea,
    secrets_wiped_at     timestamptz,
    options              jsonb not null default '{}'::jsonb check (jsonb_typeof(options) = 'object'),
    mappings             jsonb not null default '{}'::jsonb check (jsonb_typeof(mappings) = 'object'),
    -- What the connection test found (counts, gateways, registrars, servers, products, roles).
    discovery            jsonb not null default '{}'::jsonb,
    phase                text not null default 'dry_run' check (phase in ('dry_run', 'import')),
    status               text not null default 'draft' check (status in (
                             'draft', 'queued', 'running', 'completed', 'failed', 'cancelled',
                             'rollback_queued', 'rolling_back', 'rolled_back')),
    -- {"<entity>": {"state", "total", "processed", "created", "updated", "adopted", "skipped", "failed", "cursor"}}
    progress             jsonb not null default '{}'::jsonb,
    preview              jsonb not null default '{}'::jsonb,
    summary              jsonb not null default '{}'::jsonb,
    error                text,
    started_by           uuid,
    locked_by            text,
    heartbeat_at         timestamptz,
    cancel_requested_at  timestamptz,
    dry_run_at           timestamptz,
    started_at           timestamptz,
    finished_at          timestamptz,
    rolled_back_at       timestamptz,
    -- The run paused the tenant's `daily_automation` task (cut-over grace).
    automation_paused    boolean not null default false,
    created_at           timestamptz not null default now(),
    updated_at           timestamptz not null default now(),
    unique (tenant_id, id),
    constraint import_runs_started_by_fk foreign key (tenant_id, started_by)
        references users (tenant_id, id) on delete set null (started_by)
);
create index import_runs_tenant_idx on import_runs (tenant_id, created_at desc);
create unique index import_runs_one_active_idx on import_runs (tenant_id)
    where status in ('queued', 'running', 'rollback_queued', 'rolling_back');
create index import_runs_claim_idx on import_runs (status, created_at)
    where status in ('queued', 'running', 'rollback_queued', 'rolling_back');

create table import_id_map (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    source_key      text not null,
    entity          text not null check (entity ~ '^[a-z][a-z0-9_]*$'),
    source_id       text not null check (length(source_id) between 1 and 200),
    target_table    text not null check (target_table ~ '^[a-z][a-z0-9_]*$'),
    target_id       uuid not null,
    -- The run that inserted the target row (null: the row existed and was adopted).
    created_run_id  uuid,
    last_run_id     uuid,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    unique (tenant_id, source_key, entity, source_id),
    constraint import_id_map_created_run_fk foreign key (tenant_id, created_run_id)
        references import_runs (tenant_id, id) on delete set null (created_run_id),
    constraint import_id_map_last_run_fk foreign key (tenant_id, last_run_id)
        references import_runs (tenant_id, id) on delete set null (last_run_id)
);
create index import_id_map_created_run_idx on import_id_map (tenant_id, created_run_id, entity)
    where created_run_id is not null;
create index import_id_map_target_idx on import_id_map (tenant_id, target_table, target_id);

create table import_run_messages (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    run_id      uuid not null,
    phase       text not null check (phase in ('dry_run', 'import', 'rollback')),
    entity      text not null,
    level       text not null check (level in ('info', 'warning', 'conflict', 'skipped', 'error')),
    source_id   text,
    code        text not null,
    message     text not null,
    created_at  timestamptz not null default now(),
    constraint import_run_messages_run_fk foreign key (tenant_id, run_id)
        references import_runs (tenant_id, id) on delete cascade
);
create index import_run_messages_run_idx on import_run_messages (tenant_id, run_id, phase, entity, level);

select hf_attach_updated_at_triggers();
