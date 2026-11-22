-- HostForge schema, part 11: system — job queue, scheduled tasks, email
-- templates and log, inbound/outbound webhooks, in-app notifications.

-- Postgres job queue (hf-jobs). Workers claim with FOR UPDATE SKIP LOCKED.
-- Retries go back to 'pending' with a later run_at; 'failed' is final.
create table jobs (
    id            uuid primary key default hf_uuid_v7(),
    -- Null for platform-wide jobs.
    tenant_id     uuid references tenants (id) on delete cascade,
    queue         text not null default 'default' check (queue ~ '^[a-z0-9_.-]+$'),
    kind          text not null,
    payload       jsonb not null default '{}'::jsonb,
    status        text not null default 'pending'
                  check (status in ('pending', 'running', 'completed', 'failed', 'cancelled')),
    -- Higher runs first.
    priority      smallint not null default 0,
    run_at        timestamptz not null default now(),
    attempts      integer not null default 0 check (attempts >= 0),
    max_attempts  integer not null default 5 check (max_attempts > 0),
    locked_by     text,
    locked_at     timestamptz,
    last_error    text,
    result        jsonb,
    -- Global idempotency key (callers include the tenant id when it matters).
    dedupe_key    text unique,
    completed_at  timestamptz,
    failed_at     timestamptz,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    check (status <> 'running' or locked_at is not null)
);
-- Claim path: pending jobs per queue in priority / run_at order.
create index jobs_claim_idx on jobs (queue, priority desc, run_at) where status = 'pending';
-- Stuck-job recovery.
create index jobs_running_idx on jobs (locked_at) where status = 'running';
create index jobs_tenant_created_idx on jobs (tenant_id, created_at desc);
create index jobs_finished_idx on jobs (updated_at) where status in ('completed', 'failed', 'cancelled');

alter table service_events
    add constraint service_events_job_fk foreign key (job_id) references jobs (id) on delete set null;

-- Recurring tasks (cron). hf-jobs computes `next_run_at` with the `cron` crate.
create table scheduled_tasks (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid references tenants (id) on delete cascade,
    name         text not null,
    -- Job kind / queue / payload enqueued on each run.
    kind         text not null,
    queue        text not null default 'default',
    payload      jsonb not null default '{}'::jsonb,
    cron         text not null,
    timezone     text not null default 'UTC',
    is_active    boolean not null default true,
    next_run_at  timestamptz,
    last_run_at  timestamptz,
    last_status  text check (last_status in ('ok', 'error')),
    last_error   text,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now()
);
create unique index scheduled_tasks_name_idx on scheduled_tasks (tenant_id, name) nulls not distinct;
create index scheduled_tasks_due_idx on scheduled_tasks (next_run_at) where is_active;

-- Email templates per key and locale. Bodies are rendered with {{ variables }}.
create table email_templates (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    -- 'client.welcome', 'invoice.created', ...
    key         text not null check (key ~ '^[a-z0-9_]+(\.[a-z0-9_]+)*$'),
    locale      text not null check (locale in ('tr', 'en')),
    category    text not null default 'general'
                check (category in ('general', 'client', 'invoice', 'product', 'domain', 'support', 'auth', 'admin')),
    name        text not null,
    subject     text not null,
    body_html   text not null,
    body_text   text not null default '',
    is_active   boolean not null default true,
    -- Built-in (cannot be deleted, can be edited / reset).
    is_system   boolean not null default false,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    unique (tenant_id, key, locale)
);

create table email_log (
    id                   uuid primary key default hf_uuid_v7(),
    tenant_id            uuid not null references tenants (id) on delete cascade,
    user_id              uuid references users (id) on delete set null,
    client_id            uuid references clients (id) on delete set null,
    template_key         text,
    to_address           citext not null,
    cc                   text[] not null default '{}',
    bcc                  text[] not null default '{}',
    from_address         text,
    subject              text not null,
    body_html            text,
    body_text            text,
    status               text not null default 'queued' check (status in ('queued', 'sent', 'failed', 'bounced')),
    provider_message_id  text,
    error                text,
    related_type         text,
    related_id           uuid,
    sent_at              timestamptz,
    created_at           timestamptz not null default now(),
    updated_at           timestamptz not null default now()
);
create index email_log_tenant_created_idx on email_log (tenant_id, created_at desc);
create index email_log_tenant_client_idx on email_log (tenant_id, client_id, created_at desc);

-- Inbound webhooks (payment gateways, registrars). The unique key gives idempotency.
create table webhooks_in (
    id                 uuid primary key default hf_uuid_v7(),
    tenant_id          uuid not null references tenants (id) on delete cascade,
    integration_id     uuid not null references integrations (id) on delete cascade,
    external_event_id  text not null,
    event_type         text,
    payload            jsonb not null default '{}'::jsonb,
    headers            jsonb not null default '{}'::jsonb,
    signature_valid    boolean not null default false,
    status             text not null default 'received' check (status in ('received', 'processed', 'failed', 'ignored')),
    attempts           integer not null default 0,
    error              text,
    processed_at       timestamptz,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now(),
    unique (integration_id, external_event_id)
);
create index webhooks_in_tenant_created_idx on webhooks_in (tenant_id, created_at desc);
create index webhooks_in_pending_idx on webhooks_in (tenant_id, status) where status in ('received', 'failed');

-- Outbound webhook subscriptions of a tenant.
create table webhook_endpoints (
    id                uuid primary key default hf_uuid_v7(),
    tenant_id         uuid not null references tenants (id) on delete cascade,
    url               text not null check (url ~ '^https?://'),
    description       text,
    -- HMAC signing secret, SecretBox-encrypted.
    secret_enc        bytea,
    -- Event names ('invoice.paid', 'service.created', ...); empty = all.
    events            text[] not null default '{}',
    is_active         boolean not null default true,
    last_delivery_at  timestamptz,
    last_status_code  integer,
    failure_count     integer not null default 0,
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now()
);
create index webhook_endpoints_tenant_idx on webhook_endpoints (tenant_id, is_active);

create table notifications (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    user_id     uuid not null references users (id) on delete cascade,
    kind        text not null,
    title       text not null,
    body        text,
    link_url    text,
    data        jsonb not null default '{}'::jsonb,
    read_at     timestamptz,
    created_at  timestamptz not null default now()
);
create index notifications_tenant_user_idx on notifications (tenant_id, user_id, created_at desc);
create index notifications_unread_idx on notifications (tenant_id, user_id) where read_at is null;

select hf_attach_updated_at_triggers();
