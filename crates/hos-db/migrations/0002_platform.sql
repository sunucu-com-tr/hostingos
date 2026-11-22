-- HostForge schema, part 2: platform (tenants, their domains/settings, platform
-- operators, SaaS plans and subscriptions). These tables are not tenant scoped
-- (or are the tenant root itself).

create table tenants (
    id                uuid primary key default hf_uuid_v7(),
    slug              text not null unique
                      check (slug ~ '^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$'),
    name              text not null check (length(btrim(name)) > 0),
    status            text not null default 'trial'
                      check (status in ('trial', 'active', 'suspended', 'cancelled')),
    default_locale    text not null default 'tr' check (default_locale in ('tr', 'en')),
    default_currency  text not null default 'TRY' check (default_currency ~ '^[A-Z]{3}$'),
    timezone          text not null default 'Europe/Istanbul',
    -- hf_core::tenant::Branding as JSON.
    branding          jsonb not null default '{}'::jsonb check (jsonb_typeof(branding) = 'object'),
    legal_name        text,
    tax_office        text,
    tax_number        text,
    address           jsonb not null default '{}'::jsonb,
    contact_email     citext,
    contact_phone     text,
    trial_ends_at     timestamptz,
    suspended_at      timestamptz,
    suspended_reason  text,
    cancelled_at      timestamptz,
    metadata          jsonb not null default '{}'::jsonb,
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now()
);
create index tenants_status_idx on tenants (status);

-- Host names a tenant is served on: `<slug>.<HF_BASE_DOMAIN>` and custom domains.
create table tenant_domains (
    id                  uuid primary key default hf_uuid_v7(),
    tenant_id           uuid not null references tenants (id) on delete cascade,
    -- Lower-case host name without port (`demo.localhost`, `www.example.com`).
    host                text not null unique
                        check (host = lower(host) and host !~ '[/:\s]' and length(host) between 1 and 253),
    kind                text not null default 'custom' check (kind in ('subdomain', 'custom')),
    is_primary          boolean not null default false,
    verification_token  text,
    verified_at         timestamptz,
    tls_status          text not null default 'pending'
                        check (tls_status in ('pending', 'active', 'failed', 'not_required')),
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now()
);
create index tenant_domains_tenant_idx on tenant_domains (tenant_id);
create unique index tenant_domains_one_primary_idx on tenant_domains (tenant_id) where is_primary;

-- Free-form tenant settings (`billing.*`, `seo.*`, `mail.*`, ...).
create table tenant_settings (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    key         text not null check (key ~ '^[a-z0-9_]+(\.[a-z0-9_]+)*$'),
    value       jsonb not null,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    unique (tenant_id, key)
);

-- Operators of the HostForge platform itself (`/platform`). Not tenant scoped.
create table platform_admins (
    id               uuid primary key default hf_uuid_v7(),
    email            citext not null unique,
    name             text not null,
    password_hash    text,
    totp_secret_enc  bytea,
    totp_enabled     boolean not null default false,
    status           text not null default 'active' check (status in ('active', 'locked', 'disabled')),
    last_login_at    timestamptz,
    last_login_ip    inet,
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now()
);

-- SaaS plans sold to tenants.
create table platform_plans (
    id             uuid primary key default hf_uuid_v7(),
    code           text not null unique check (code ~ '^[a-z0-9_-]+$'),
    name           text not null,
    description    text,
    currency       text not null default 'USD' check (currency ~ '^[A-Z]{3}$'),
    price_monthly  numeric(20, 6) not null default 0 check (price_monthly >= 0),
    price_annual   numeric(20, 6) not null default 0 check (price_annual >= 0),
    trial_days     integer not null default 14 check (trial_days >= 0),
    -- {"max_clients": 500, "max_services": 1000, "max_staff": 5, "custom_domains": 1, ...}; null = unlimited.
    limits         jsonb not null default '{}'::jsonb,
    -- ["white_label", "api", "ai_content", ...]
    features       jsonb not null default '[]'::jsonb,
    is_public      boolean not null default true,
    is_active      boolean not null default true,
    sort_order     integer not null default 0,
    created_at     timestamptz not null default now(),
    updated_at     timestamptz not null default now()
);

create table tenant_subscriptions (
    id                    uuid primary key default hf_uuid_v7(),
    tenant_id             uuid not null references tenants (id) on delete cascade,
    plan_id               uuid not null references platform_plans (id),
    status                text not null default 'trialing'
                          check (status in ('trialing', 'active', 'past_due', 'cancelled', 'expired')),
    billing_cycle         text not null default 'monthly' check (billing_cycle in ('monthly', 'annual')),
    price                 numeric(20, 6) not null default 0,
    currency              text not null default 'USD' check (currency ~ '^[A-Z]{3}$'),
    current_period_start  timestamptz,
    current_period_end    timestamptz,
    trial_ends_at         timestamptz,
    cancel_at_period_end  boolean not null default false,
    cancelled_at          timestamptz,
    external_ref          text,
    created_at            timestamptz not null default now(),
    updated_at            timestamptz not null default now()
);
create index tenant_subscriptions_tenant_idx on tenant_subscriptions (tenant_id, created_at desc);
create unique index tenant_subscriptions_one_live_idx
    on tenant_subscriptions (tenant_id) where status in ('trialing', 'active', 'past_due');

select hf_attach_updated_at_triggers();
