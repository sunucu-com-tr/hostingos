-- HostForge schema, part 7: provisioned services, their addons and event log,
-- virtual machines and IP address management.

create table services (
    id                         uuid primary key default hf_uuid_v7(),
    tenant_id                  uuid not null references tenants (id) on delete cascade,
    client_id                  uuid not null references clients (id),
    product_id                 uuid not null references products (id),
    order_id                   uuid references orders (id) on delete set null,
    status                     text not null default 'pending' check (status in (
                                   'pending', 'provisioning', 'active', 'suspended', 'terminated',
                                   'cancelled', 'failed')),
    billing_cycle              text not null check (billing_cycle in (
                                   'monthly', 'quarterly', 'semiannual', 'annual', 'biennial', 'triennial', 'one_time')),
    first_payment_amount       numeric(20, 6) not null default 0,
    recurring_amount           numeric(20, 6) not null default 0,
    currency                   text not null check (currency ~ '^[A-Z]{3}$'),
    quantity                   integer not null default 1 check (quantity > 0),
    registration_date          date not null default current_date,
    -- Next renewal due date (null for one-time / free services).
    next_due_date              date,
    -- When the renewal invoice for `next_due_date` should be generated (due date - N days).
    next_invoice_date          date,
    -- Original day of month for anchored due dates (BillingCycle::next_due_anchored).
    anchor_day                 smallint check (anchor_day between 1 and 31),
    integration_id             uuid references integrations (id) on delete set null,
    -- Provider-side id (cPanel username, Proxmox vmid, Hetzner server id, ...).
    external_id                text,
    username                   text,
    password_enc               bytea,
    domain                     text,
    server_hostname            text,
    dedicated_ip               inet,
    assigned_ips               inet[] not null default '{}',
    -- Provider data the module wants to keep (node, package, plan ids, ...).
    module_data                jsonb not null default '{}'::jsonb,
    -- {"<config_option_id>": {"value_id": "<uuid>", "quantity": n}}
    config_selections          jsonb not null default '{}'::jsonb,
    custom_fields              jsonb not null default '{}'::jsonb,
    notes                      text,
    admin_notes                text,
    -- Staff override: do not auto-suspend before this date.
    override_suspend_until     date,
    suspended_at               timestamptz,
    suspended_reason           text,
    terminated_at              timestamptz,
    cancelled_at               timestamptz,
    cancellation_requested_at  timestamptz,
    cancellation_type          text check (cancellation_type in ('immediate', 'end_of_period')),
    cancellation_reason        text,
    created_at                 timestamptz not null default now(),
    updated_at                 timestamptz not null default now()
);
create index services_tenant_client_idx on services (tenant_id, client_id, created_at desc);
create index services_tenant_status_due_idx on services (tenant_id, status, next_due_date);
create index services_tenant_product_idx on services (tenant_id, product_id);
create index services_integration_external_idx on services (integration_id, external_id)
    where external_id is not null;
create index services_billable_invoice_date_idx on services (next_invoice_date)
    where status in ('active', 'suspended');

create table service_addons (
    id                 uuid primary key default hf_uuid_v7(),
    tenant_id          uuid not null references tenants (id) on delete cascade,
    service_id         uuid not null references services (id) on delete cascade,
    addon_id           uuid not null references product_addons (id),
    order_id           uuid references orders (id) on delete set null,
    status             text not null default 'pending' check (status in (
                           'pending', 'provisioning', 'active', 'suspended', 'terminated',
                           'cancelled', 'failed')),
    billing_cycle      text not null check (billing_cycle in (
                           'monthly', 'quarterly', 'semiannual', 'annual', 'biennial', 'triennial', 'one_time')),
    setup_fee          numeric(20, 6) not null default 0,
    recurring_amount   numeric(20, 6) not null default 0,
    currency           text not null check (currency ~ '^[A-Z]{3}$'),
    registration_date  date not null default current_date,
    next_due_date      date,
    next_invoice_date  date,
    module_data        jsonb not null default '{}'::jsonb,
    notes              text,
    suspended_at       timestamptz,
    terminated_at      timestamptz,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now()
);
create index service_addons_tenant_service_idx on service_addons (tenant_id, service_id);
create index service_addons_tenant_status_due_idx on service_addons (tenant_id, status, next_due_date);

-- Lifecycle / provisioning log of a service (docs/ARCHITECTURE.md §4).
create table service_events (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    service_id   uuid not null references services (id) on delete cascade,
    -- e.g. 'created', 'provision_requested', 'provisioned', 'suspended', 'password_changed'.
    event        text not null,
    level        text not null default 'info' check (level in ('info', 'warning', 'error')),
    message      text not null default '',
    from_status  text,
    to_status    text,
    -- Job that produced the event (FK added with the jobs table).
    job_id       uuid,
    actor_kind   text check (actor_kind in ('user', 'platform_admin', 'system', 'api_key')),
    actor_id     uuid,
    data         jsonb not null default '{}'::jsonb,
    created_at   timestamptz not null default now()
);
create index service_events_tenant_service_idx on service_events (tenant_id, service_id, created_at desc);

-- Service ↔ virtual machine mapping (virt integrations).
create table vms (
    id                 uuid primary key default hf_uuid_v7(),
    tenant_id          uuid not null references tenants (id) on delete cascade,
    service_id         uuid references services (id) on delete set null,
    integration_id     uuid not null references integrations (id),
    node               text,
    -- Provider VM id (Proxmox vmid, vSphere moref, Hetzner server id, ...).
    external_id        text not null,
    name               text not null,
    hostname           text,
    os_template        text,
    cpu_cores          integer check (cpu_cores > 0),
    memory_mb          integer check (memory_mb > 0),
    disk_gb            integer check (disk_gb > 0),
    bandwidth_gb       integer,
    status             text not null default 'unknown' check (status in (
                           'running', 'stopped', 'paused', 'suspended', 'starting', 'stopping',
                           'provisioning', 'migrating', 'error', 'unknown')),
    primary_ipv4       inet,
    primary_ipv6       inet,
    root_password_enc  bytea,
    metadata           jsonb not null default '{}'::jsonb,
    last_synced_at     timestamptz,
    deleted_at         timestamptz,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now(),
    unique (integration_id, external_id)
);
create index vms_tenant_service_idx on vms (tenant_id, service_id);
create index vms_tenant_integration_idx on vms (tenant_id, integration_id);

create table ip_pools (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    integration_id  uuid references integrations (id) on delete set null,
    name            text not null,
    network         cidr not null,
    gateway         inet,
    nameservers     inet[] not null default '{}',
    node            text,
    vlan            integer check (vlan between 1 and 4094),
    is_active       boolean not null default true,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    unique (tenant_id, name)
);

create table ip_addresses (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    pool_id      uuid not null references ip_pools (id) on delete cascade,
    address      inet not null,
    status       text not null default 'free' check (status in ('free', 'reserved', 'assigned', 'blocked')),
    service_id   uuid references services (id) on delete set null,
    vm_id        uuid references vms (id) on delete set null,
    mac_address  text,
    ptr          text,
    notes        text,
    assigned_at  timestamptz,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    unique (pool_id, address),
    check (status <> 'assigned' or service_id is not null or vm_id is not null)
);
create index ip_addresses_tenant_pool_status_idx on ip_addresses (tenant_id, pool_id, status);
create index ip_addresses_tenant_service_idx on ip_addresses (tenant_id, service_id) where service_id is not null;

-- Deferred foreign keys from the sales tables.
alter table order_items
    add constraint order_items_service_fk foreign key (service_id) references services (id) on delete set null,
    add constraint order_items_service_addon_fk foreign key (service_addon_id) references service_addons (id) on delete set null;
alter table invoice_items
    add constraint invoice_items_service_fk foreign key (service_id) references services (id) on delete set null,
    add constraint invoice_items_service_addon_fk foreign key (service_addon_id) references service_addons (id) on delete set null;
create index invoice_items_service_idx on invoice_items (service_id) where service_id is not null;

select hf_attach_updated_at_triggers();
