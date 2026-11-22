-- HostForge schema, part 5: integrations (provider modules) and the product
-- catalog: currencies, taxes, product groups, products, pricing, configurable
-- options, addons and coupons.
--
-- Translatable rows keep their default-locale text in plain columns and other
-- locales in `translations jsonb`: {"en": {"name": "...", "description": "..."}}.

-- Configured provider modules (docs/ARCHITECTURE.md §4).
create table integrations (
    id                 uuid primary key default hf_uuid_v7(),
    tenant_id          uuid not null references tenants (id) on delete cascade,
    category           text not null
                       check (category in ('virt', 'panel', 'registrar', 'gateway', 'mail', 'ai', 'oauth')),
    kind               text not null check (kind ~ '^[a-z0-9_]+$'),
    name               text not null,
    -- Plain settings keyed by FieldSpec::key.
    config             jsonb not null default '{}'::jsonb check (jsonb_typeof(config) = 'object'),
    -- SecretBox(JSON) with AAD = hf_core::crypto::integration_aad(tenant_id, id).
    secrets_enc        bytea,
    is_active          boolean not null default true,
    -- Default integration of its category (e.g. the mailer, the storefront gateway list order).
    is_default         boolean not null default false,
    sort_order         integer not null default 0,
    last_test_at       timestamptz,
    last_test_ok       boolean,
    last_test_message  text,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now(),
    unique (tenant_id, category, name)
);
create index integrations_tenant_category_idx on integrations (tenant_id, category, sort_order);
create unique index integrations_one_default_idx on integrations (tenant_id, category) where is_default;

alter table clients
    add constraint clients_default_gateway_fk
    foreign key (default_gateway_id) references integrations (id) on delete set null;

create table currencies (
    id             uuid primary key default hf_uuid_v7(),
    tenant_id      uuid not null references tenants (id) on delete cascade,
    code           text not null check (code ~ '^[A-Z]{3}$'),
    name           text not null,
    symbol         text not null default '',
    -- Rendering: prefix + amount + suffix, e.g. '₺' + '1.234,56' or '$' + '1,234.56'.
    prefix         text not null default '',
    suffix         text not null default '',
    decimals       smallint not null default 2 check (decimals between 0 and 6),
    -- Value of 1 unit of the tenant default currency in this currency.
    exchange_rate  numeric(20, 10) not null default 1 check (exchange_rate > 0),
    is_default     boolean not null default false,
    is_active      boolean not null default true,
    sort_order     integer not null default 0,
    created_at     timestamptz not null default now(),
    updated_at     timestamptz not null default now(),
    unique (tenant_id, code)
);
create unique index currencies_one_default_idx on currencies (tenant_id) where is_default;

-- Tax rules (WHMCS tax levels). Level 2 rules may be compound (applied on top of level 1).
create table tax_rules (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    name          text not null,
    level         smallint not null default 1 check (level in (1, 2)),
    -- Null = every country / region.
    country_code  text check (country_code ~ '^[A-Z]{2}$'),
    region        text,
    rate          numeric(9, 4) not null check (rate >= 0 and rate <= 100),
    is_compound   boolean not null default false,
    is_active     boolean not null default true,
    priority      integer not null default 0,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now()
);
create index tax_rules_tenant_idx on tax_rules (tenant_id, is_active, level, priority);

create table product_groups (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    slug            text not null check (slug ~ '^[a-z0-9]+(?:-[a-z0-9]+)*$'),
    name            text not null,
    tagline         text,
    description     text,
    translations    jsonb not null default '{}'::jsonb,
    icon            text,
    order_template  text not null default 'standard',
    is_hidden       boolean not null default false,
    sort_order      integer not null default 0,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    unique (tenant_id, slug)
);
create index product_groups_tenant_sort_idx on product_groups (tenant_id, sort_order);

create table products (
    id                      uuid primary key default hf_uuid_v7(),
    tenant_id               uuid not null references tenants (id) on delete cascade,
    group_id                uuid references product_groups (id) on delete set null,
    slug                    text not null check (slug ~ '^[a-z0-9]+(?:-[a-z0-9]+)*$'),
    product_type            text not null check (product_type in (
                                'shared_hosting', 'reseller_hosting', 'vps', 'dedicated', 'cloud',
                                'domain', 'ssl', 'license', 'email', 'ai_service', 'managed_db',
                                'object_storage', 'container_app', 'other')),
    name                    text not null,
    tagline                 text,
    description             text,
    -- Bullet list for pricing cards: ["10 GB NVMe", "Free SSL", ...].
    features                jsonb not null default '[]'::jsonb,
    translations            jsonb not null default '{}'::jsonb,
    integration_id          uuid references integrations (id) on delete set null,
    -- Module settings (panel package name, VM template, plan code, ...).
    module_config           jsonb not null default '{}'::jsonb,
    pay_type                text not null default 'recurring' check (pay_type in ('free', 'one_time', 'recurring')),
    auto_setup              text not null default 'on_payment'
                            check (auto_setup in ('on_order', 'on_payment', 'on_accept', 'manual')),
    requires_domain         text not null default 'none' check (requires_domain in ('none', 'optional', 'required')),
    welcome_email_template  text,
    stock_control           boolean not null default false,
    stock_quantity          integer not null default 0,
    allow_quantity          boolean not null default false,
    is_taxable              boolean not null default true,
    is_featured             boolean not null default false,
    is_hidden               boolean not null default false,
    is_retired              boolean not null default false,
    -- Products this one can be upgraded/downgraded to.
    upgrade_product_ids     uuid[] not null default '{}',
    sort_order              integer not null default 0,
    created_at              timestamptz not null default now(),
    updated_at              timestamptz not null default now(),
    unique (tenant_id, slug)
);
create index products_tenant_group_idx on products (tenant_id, group_id, sort_order);
create index products_tenant_type_idx on products (tenant_id, product_type);

-- Price per currency × billing cycle (+ setup fee).
create table product_pricing (
    id             uuid primary key default hf_uuid_v7(),
    tenant_id      uuid not null references tenants (id) on delete cascade,
    product_id     uuid not null references products (id) on delete cascade,
    currency       text not null check (currency ~ '^[A-Z]{3}$'),
    billing_cycle  text not null check (billing_cycle in (
                       'monthly', 'quarterly', 'semiannual', 'annual', 'biennial', 'triennial', 'one_time')),
    price          numeric(20, 6) not null check (price >= 0),
    setup_fee      numeric(20, 6) not null default 0 check (setup_fee >= 0),
    is_enabled     boolean not null default true,
    created_at     timestamptz not null default now(),
    updated_at     timestamptz not null default now(),
    unique (product_id, currency, billing_cycle)
);
create index product_pricing_tenant_product_idx on product_pricing (tenant_id, product_id);

create table config_option_groups (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    name         text not null,
    description  text,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    unique (tenant_id, name)
);

create table config_option_group_products (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    group_id    uuid not null references config_option_groups (id) on delete cascade,
    product_id  uuid not null references products (id) on delete cascade,
    created_at  timestamptz not null default now(),
    unique (group_id, product_id)
);
create index config_option_group_products_tenant_product_idx
    on config_option_group_products (tenant_id, product_id);

create table config_options (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    group_id      uuid not null references config_option_groups (id) on delete cascade,
    name          text not null,
    translations  jsonb not null default '{}'::jsonb,
    option_type   text not null default 'dropdown'
                  check (option_type in ('dropdown', 'radio', 'yes_no', 'quantity')),
    -- Provisioning key passed to the module (e.g. 'cpu_cores', 'memory_mb', 'os_template').
    module_key    text,
    unit          text,
    min_quantity  integer not null default 0,
    max_quantity  integer not null default 0,
    is_hidden     boolean not null default false,
    sort_order    integer not null default 0,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now()
);
create index config_options_tenant_group_idx on config_options (tenant_id, group_id, sort_order);

create table config_option_values (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    option_id     uuid not null references config_options (id) on delete cascade,
    label         text not null,
    translations  jsonb not null default '{}'::jsonb,
    -- Value passed to the module (e.g. '4', 'ubuntu-24.04').
    value         text not null default '',
    is_hidden     boolean not null default false,
    sort_order    integer not null default 0,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now()
);
create index config_option_values_tenant_option_idx on config_option_values (tenant_id, option_id, sort_order);

create table config_option_pricing (
    id             uuid primary key default hf_uuid_v7(),
    tenant_id      uuid not null references tenants (id) on delete cascade,
    value_id       uuid not null references config_option_values (id) on delete cascade,
    currency       text not null check (currency ~ '^[A-Z]{3}$'),
    billing_cycle  text not null check (billing_cycle in (
                       'monthly', 'quarterly', 'semiannual', 'annual', 'biennial', 'triennial', 'one_time')),
    -- For quantity options: price per unit.
    price          numeric(20, 6) not null default 0,
    setup_fee      numeric(20, 6) not null default 0,
    created_at     timestamptz not null default now(),
    updated_at     timestamptz not null default now(),
    unique (value_id, currency, billing_cycle)
);
create index config_option_pricing_tenant_value_idx on config_option_pricing (tenant_id, value_id);

create table product_addons (
    id                      uuid primary key default hf_uuid_v7(),
    tenant_id               uuid not null references tenants (id) on delete cascade,
    slug                    text not null check (slug ~ '^[a-z0-9]+(?:-[a-z0-9]+)*$'),
    name                    text not null,
    description             text,
    translations            jsonb not null default '{}'::jsonb,
    integration_id          uuid references integrations (id) on delete set null,
    module_config           jsonb not null default '{}'::jsonb,
    pay_type                text not null default 'recurring' check (pay_type in ('free', 'one_time', 'recurring')),
    auto_activate           boolean not null default true,
    show_on_order           boolean not null default true,
    welcome_email_template  text,
    is_taxable              boolean not null default true,
    is_hidden               boolean not null default false,
    sort_order              integer not null default 0,
    created_at              timestamptz not null default now(),
    updated_at              timestamptz not null default now(),
    unique (tenant_id, slug)
);

create table product_addon_products (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    addon_id    uuid not null references product_addons (id) on delete cascade,
    product_id  uuid not null references products (id) on delete cascade,
    created_at  timestamptz not null default now(),
    unique (addon_id, product_id)
);
create index product_addon_products_tenant_product_idx on product_addon_products (tenant_id, product_id);

create table product_addon_pricing (
    id             uuid primary key default hf_uuid_v7(),
    tenant_id      uuid not null references tenants (id) on delete cascade,
    addon_id       uuid not null references product_addons (id) on delete cascade,
    currency       text not null check (currency ~ '^[A-Z]{3}$'),
    billing_cycle  text not null check (billing_cycle in (
                       'monthly', 'quarterly', 'semiannual', 'annual', 'biennial', 'triennial', 'one_time')),
    price          numeric(20, 6) not null check (price >= 0),
    setup_fee      numeric(20, 6) not null default 0 check (setup_fee >= 0),
    created_at     timestamptz not null default now(),
    updated_at     timestamptz not null default now(),
    unique (addon_id, currency, billing_cycle)
);
create index product_addon_pricing_tenant_addon_idx on product_addon_pricing (tenant_id, addon_id);

create table coupons (
    id                 uuid primary key default hf_uuid_v7(),
    tenant_id          uuid not null references tenants (id) on delete cascade,
    code               citext not null,
    description        text,
    discount_type      text not null check (discount_type in ('percent', 'fixed', 'price_override', 'free_setup')),
    -- Percent (0-100) or an amount in `currency`.
    value              numeric(20, 6) not null default 0 check (value >= 0),
    currency           text check (currency ~ '^[A-Z]{3}$'),
    -- Empty arrays = applies to everything of that kind.
    product_ids        uuid[] not null default '{}',
    addon_ids          uuid[] not null default '{}',
    tlds               text[] not null default '{}',
    billing_cycles     text[] not null default '{}',
    applies_to_setup   boolean not null default false,
    recurring          boolean not null default false,
    -- Number of renewals the discount applies to when recurring (0 = forever).
    recurring_limit    integer not null default 0,
    new_clients_only   boolean not null default false,
    once_per_client    boolean not null default false,
    min_order_amount   numeric(20, 6),
    max_uses           integer check (max_uses is null or max_uses > 0),
    uses_count         integer not null default 0 check (uses_count >= 0),
    starts_at          timestamptz,
    expires_at         timestamptz,
    is_active          boolean not null default true,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now(),
    unique (tenant_id, code),
    check (discount_type <> 'percent' or value <= 100),
    check (discount_type not in ('fixed', 'price_override') or currency is not null)
);

select hf_attach_updated_at_triggers();
