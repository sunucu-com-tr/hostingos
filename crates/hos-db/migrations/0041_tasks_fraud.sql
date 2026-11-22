-- 0041: staff to-do list and the order screening ban list.
--
--   * staff_tasks — to-dos with due date, priority, assignee and an optional
--                   client; completed ones keep `completed_at`.
--   * fraud_bans  — blocked e-mail addresses, e-mail domains and IP
--                   addresses / networks used by the built-in order
--                   screening rules (hos-billing `fraud`).

create table staff_tasks (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    title         text not null check (length(title) between 1 and 200),
    body          text check (body is null or length(body) <= 10000),
    status        text not null default 'open' check (status in ('open', 'done')),
    priority      text not null default 'normal' check (priority in ('low', 'normal', 'high')),
    due_date      date,
    assigned_to   uuid,
    client_id     uuid,
    created_by    uuid references users (id) on delete set null,
    completed_at  timestamptz,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    unique (tenant_id, id),
    constraint staff_tasks_assignee_fk foreign key (tenant_id, assigned_to)
        references users (tenant_id, id) on delete set null (assigned_to),
    constraint staff_tasks_client_fk foreign key (tenant_id, client_id)
        references clients (tenant_id, id) on delete cascade
);
create index staff_tasks_tenant_open_idx on staff_tasks (tenant_id, status, due_date);
create index staff_tasks_tenant_client_idx on staff_tasks (tenant_id, client_id) where client_id is not null;

create table fraud_bans (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    kind        text not null check (kind in ('email', 'domain', 'ip')),
    -- Lower-case e-mail / domain, or an address / network in CIDR notation.
    value       text not null check (length(value) between 1 and 254),
    reason      text check (reason is null or length(reason) <= 500),
    created_by  uuid references users (id) on delete set null,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    unique (tenant_id, kind, value)
);
create index fraud_bans_tenant_idx on fraud_bans (tenant_id, kind);

select hf_attach_updated_at_triggers();
