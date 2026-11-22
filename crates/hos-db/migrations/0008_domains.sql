-- HostForge schema, part 8: domain names — sellable TLDs and their prices,
-- registered domains, contacts, DNS zones and records.

create table tlds (
    id                         uuid primary key default hf_uuid_v7(),
    tenant_id                  uuid not null references tenants (id) on delete cascade,
    -- Lower-case, without the leading dot: 'com', 'com.tr'.
    tld                        text not null check (tld ~ '^[a-z0-9-]+(\.[a-z0-9-]+)*$'),
    registrar_integration_id   uuid references integrations (id) on delete set null,
    requires_epp               boolean not null default true,
    supports_privacy           boolean not null default true,
    supports_dns_management    boolean not null default true,
    -- Registry-specific registration requirements (documents, local presence, ...).
    requirements               jsonb not null default '{}'::jsonb,
    min_years                  smallint not null default 1 check (min_years between 1 and 10),
    max_years                  smallint not null default 10 check (max_years between 1 and 10),
    grace_period_days          integer not null default 0,
    redemption_period_days     integer not null default 0,
    -- Storefront label: 'popular', 'new', 'sale', 'country', ...
    label                      text,
    is_featured                boolean not null default false,
    is_active                  boolean not null default true,
    sort_order                 integer not null default 0,
    created_at                 timestamptz not null default now(),
    updated_at                 timestamptz not null default now(),
    unique (tenant_id, tld),
    check (min_years <= max_years)
);
create index tlds_tenant_sort_idx on tlds (tenant_id, is_active, sort_order);

-- Price of a TLD per currency × action × number of years.
create table tld_pricing (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    tld_id      uuid not null references tlds (id) on delete cascade,
    currency    text not null check (currency ~ '^[A-Z]{3}$'),
    action      text not null check (action in ('register', 'transfer', 'renew', 'restore')),
    years       smallint not null check (years between 1 and 10),
    price       numeric(20, 6) not null check (price >= 0),
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    unique (tld_id, currency, action, years)
);
create index tld_pricing_tenant_tld_idx on tld_pricing (tenant_id, tld_id);

create table domains (
    id                        uuid primary key default hf_uuid_v7(),
    tenant_id                 uuid not null references tenants (id) on delete cascade,
    client_id                 uuid not null references clients (id),
    order_id                  uuid references orders (id) on delete set null,
    -- Fully qualified, lower-case (IDNs in punycode).
    name                      citext not null check (name::text = lower(name::text) and position('.' in name) > 1),
    tld                       text not null,
    registrar_integration_id  uuid references integrations (id) on delete set null,
    -- Registrar order / object id.
    external_id               text,
    status                    text not null default 'pending' check (status in (
                                  'pending', 'pending_registration', 'pending_transfer', 'active', 'grace',
                                  'redemption', 'expired', 'transferred_away', 'cancelled', 'fraud')),
    registration_type         text not null default 'register' check (registration_type in ('register', 'transfer')),
    registration_period       smallint not null default 1 check (registration_period between 1 and 10),
    registered_at             timestamptz,
    expires_at                timestamptz,
    next_due_date             date,
    next_invoice_date         date,
    first_payment_amount      numeric(20, 6) not null default 0,
    recurring_amount          numeric(20, 6) not null default 0,
    currency                  text not null check (currency ~ '^[A-Z]{3}$'),
    auto_renew                boolean not null default true,
    privacy                   boolean not null default false,
    lock                      boolean not null default true,
    dns_management            boolean not null default false,
    email_forwarding          boolean not null default false,
    nameservers               text[] not null default '{}',
    -- EPP / auth code, SecretBox-encrypted.
    epp_code_enc              bytea,
    last_synced_at            timestamptz,
    sync_error                text,
    notes                     text,
    admin_notes               text,
    cancelled_at              timestamptz,
    created_at                timestamptz not null default now(),
    updated_at                timestamptz not null default now()
);
-- A name can exist only once per tenant while it is live.
create unique index domains_tenant_name_live_idx on domains (tenant_id, name)
    where status not in ('expired', 'transferred_away', 'cancelled', 'fraud');
create index domains_tenant_client_idx on domains (tenant_id, client_id, created_at desc);
create index domains_tenant_status_expiry_idx on domains (tenant_id, status, expires_at);
create index domains_registrar_external_idx on domains (registrar_integration_id, external_id)
    where external_id is not null;

create table domain_contacts (
    id                   uuid primary key default hf_uuid_v7(),
    tenant_id            uuid not null references tenants (id) on delete cascade,
    domain_id            uuid not null references domains (id) on delete cascade,
    role                 text not null check (role in ('registrant', 'admin', 'tech', 'billing')),
    first_name           text not null default '',
    last_name            text not null default '',
    organization         text,
    email                citext not null,
    phone                text,
    address_line1        text,
    address_line2        text,
    city                 text,
    state                text,
    postal_code          text,
    country_code         text not null check (country_code ~ '^[A-Z]{2}$'),
    external_contact_id  text,
    created_at           timestamptz not null default now(),
    updated_at           timestamptz not null default now(),
    unique (domain_id, role)
);
create index domain_contacts_tenant_domain_idx on domain_contacts (tenant_id, domain_id);

create table dns_zones (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    domain_id       uuid references domains (id) on delete set null,
    client_id       uuid references clients (id) on delete cascade,
    -- Zone apex, lower-case.
    name            citext not null,
    integration_id  uuid references integrations (id) on delete set null,
    external_id     text,
    serial          bigint not null default 1,
    default_ttl     integer not null default 3600 check (default_ttl > 0),
    is_active       boolean not null default true,
    last_synced_at  timestamptz,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    unique (tenant_id, name)
);
create index dns_zones_tenant_client_idx on dns_zones (tenant_id, client_id);

create table dns_records (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    zone_id      uuid not null references dns_zones (id) on delete cascade,
    -- Relative name ('@', 'www', '_dmarc').
    name         text not null,
    record_type  text not null check (record_type in ('A', 'AAAA', 'CNAME', 'MX', 'TXT', 'NS', 'SRV', 'CAA')),
    content      text not null,
    ttl          integer not null default 3600 check (ttl > 0),
    priority     integer check (priority >= 0),
    external_id  text,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now()
);
create index dns_records_tenant_zone_idx on dns_records (tenant_id, zone_id, name, record_type);

alter table order_items
    add constraint order_items_domain_fk foreign key (domain_id) references domains (id) on delete set null;
alter table invoice_items
    add constraint invoice_items_domain_fk foreign key (domain_id) references domains (id) on delete set null;
create index invoice_items_domain_idx on invoice_items (domain_id) where domain_id is not null;

select hf_attach_updated_at_triggers();
