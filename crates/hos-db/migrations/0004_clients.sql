-- HostForge schema, part 4: customers (billing profiles), their login users,
-- contacts, notes and groups.

create table client_groups (
    id                uuid primary key default hf_uuid_v7(),
    tenant_id         uuid not null references tenants (id) on delete cascade,
    name              text not null,
    description       text,
    color             text check (color ~ '^#[0-9a-fA-F]{3,8}$'),
    discount_percent  numeric(9, 4) not null default 0 check (discount_percent between 0 and 100),
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now(),
    unique (tenant_id, name)
);

-- A billing customer (person or company). Login accounts are linked via client_users.
create table clients (
    id                 uuid primary key default hf_uuid_v7(),
    tenant_id          uuid not null references tenants (id) on delete cascade,
    kind               text not null default 'individual' check (kind in ('individual', 'corporate')),
    first_name         text not null default '',
    last_name          text not null default '',
    company_name       text,
    email              citext not null check (position('@' in email) > 1),
    phone              text,
    address_line1      text,
    address_line2      text,
    city               text,
    state              text,
    postal_code        text,
    country_code       text not null default 'TR' check (country_code ~ '^[A-Z]{2}$'),
    -- Turkish invoicing: tax office + VKN for companies, TCKN for individuals.
    tax_office         text,
    tax_number         text,
    national_id        text,
    currency           text not null default 'TRY' check (currency ~ '^[A-Z]{3}$'),
    locale             text not null default 'tr' check (locale in ('tr', 'en')),
    group_id           uuid references client_groups (id) on delete set null,
    status             text not null default 'active' check (status in ('active', 'inactive', 'closed')),
    tax_exempt         boolean not null default false,
    late_fee_exempt    boolean not null default false,
    overdue_notices    boolean not null default true,
    separate_invoices  boolean not null default false,
    marketing_opt_in   boolean not null default false,
    -- Default payment integration (gateway) for new invoices.
    default_gateway_id uuid,
    custom_fields      jsonb not null default '{}'::jsonb,
    signup_ip          inet,
    closed_at          timestamptz,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now()
);
create index clients_tenant_created_idx on clients (tenant_id, created_at desc);
create index clients_tenant_email_idx on clients (tenant_id, email);
create index clients_tenant_status_idx on clients (tenant_id, status);
create index clients_tenant_group_idx on clients (tenant_id, group_id) where group_id is not null;

-- Which users may act for which client (WHMCS "users" ↔ "client accounts").
create table client_users (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    client_id    uuid not null references clients (id) on delete cascade,
    user_id      uuid not null references users (id) on delete cascade,
    is_owner     boolean not null default false,
    -- Empty = full access; otherwise e.g. {'invoices','tickets','services','domains','profile','contacts','orders'}.
    permissions  text[] not null default '{}',
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    unique (client_id, user_id)
);
create index client_users_tenant_user_idx on client_users (tenant_id, user_id);
create unique index client_users_one_owner_idx on client_users (client_id) where is_owner;

create table contacts (
    id                 uuid primary key default hf_uuid_v7(),
    tenant_id          uuid not null references tenants (id) on delete cascade,
    client_id          uuid not null references clients (id) on delete cascade,
    first_name         text not null default '',
    last_name          text not null default '',
    company_name       text,
    email              citext not null,
    phone              text,
    address_line1      text,
    address_line2      text,
    city               text,
    state              text,
    postal_code        text,
    country_code       text not null default 'TR' check (country_code ~ '^[A-Z]{2}$'),
    is_billing         boolean not null default false,
    is_domain_contact  boolean not null default false,
    -- Email categories this contact receives: general, invoice, support, product, domain.
    email_preferences  text[] not null default '{}',
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now()
);
create index contacts_tenant_client_idx on contacts (tenant_id, client_id);

create table client_notes (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    client_id       uuid not null references clients (id) on delete cascade,
    author_user_id  uuid references users (id) on delete set null,
    body            text not null,
    is_sticky       boolean not null default false,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now()
);
create index client_notes_tenant_client_idx on client_notes (tenant_id, client_id, created_at desc);

select hf_attach_updated_at_triggers();
