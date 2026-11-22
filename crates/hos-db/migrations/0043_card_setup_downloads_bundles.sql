-- 0043: saving a card without paying, the downloads area and product bundles.

-- ---------------------------------------------------------------------------
-- Card setup without a payment (Stripe Checkout `mode=setup`).
--
-- One row per hand-off to the gateway. The gateway echoes `reference`; the
-- signed completion event stores the payment method once (`status`).
-- ---------------------------------------------------------------------------
create table payment_setups (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    client_id       uuid not null,
    integration_id  uuid not null,
    -- Merchant reference sent to the gateway (alphanumeric, unguessable).
    reference       text not null check (reference ~ '^[A-Za-z0-9]{16,64}$'),
    -- Gateway session id (Stripe `cs_…`).
    gateway_ref     text,
    status          text not null default 'pending'
                    check (status in ('pending', 'succeeded', 'failed', 'expired')),
    created_by      uuid references users (id) on delete set null,
    completed_at    timestamptz,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    unique (tenant_id, id),
    unique (tenant_id, reference),
    constraint payment_setups_client_fk foreign key (tenant_id, client_id)
        references clients (tenant_id, id) on delete cascade,
    constraint payment_setups_integration_fk foreign key (tenant_id, integration_id)
        references integrations (tenant_id, id) on delete cascade
);
create index payment_setups_tenant_client_idx on payment_setups (tenant_id, client_id, created_at desc);

-- ---------------------------------------------------------------------------
-- Downloads area (client area / storefront).
-- ---------------------------------------------------------------------------
create table download_categories (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    name         text not null check (length(name) between 1 and 200),
    description  text,
    is_hidden    boolean not null default false,
    sort_order   integer not null default 0,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    unique (tenant_id, id)
);

create table downloads (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    category_id     uuid,
    title           text not null check (length(title) between 1 and 200),
    description     text,
    -- Sanitised file name offered to the browser.
    filename        text not null check (length(filename) between 1 and 200),
    content_type    text not null default 'application/octet-stream',
    size_bytes      bigint not null default 0 check (size_bytes >= 0),
    sha256          text check (sha256 ~ '^[0-9a-f]{64}$'),
    -- public: anyone; clients: signed-in clients; product: clients with an
    -- active service of a linked product.
    access          text not null default 'clients' check (access in ('public', 'clients', 'product')),
    is_hidden       boolean not null default false,
    download_count  bigint not null default 0,
    sort_order      integer not null default 0,
    created_by      uuid references users (id) on delete set null,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    unique (tenant_id, id),
    constraint downloads_category_fk foreign key (tenant_id, category_id)
        references download_categories (tenant_id, id) on delete set null (category_id)
);
create index downloads_tenant_category_idx on downloads (tenant_id, category_id, sort_order);

-- File bytes, kept apart so listings never load them. A download without a
-- row here (e.g. imported metadata) is not served.
create table download_files (
    download_id  uuid primary key,
    tenant_id    uuid not null references tenants (id) on delete cascade,
    content      bytea not null,
    created_at   timestamptz not null default now(),
    constraint download_files_download_fk foreign key (tenant_id, download_id)
        references downloads (tenant_id, id) on delete cascade
);

create table download_products (
    tenant_id    uuid not null references tenants (id) on delete cascade,
    download_id  uuid not null,
    product_id   uuid not null,
    primary key (download_id, product_id),
    constraint download_products_download_fk foreign key (tenant_id, download_id)
        references downloads (tenant_id, id) on delete cascade,
    constraint download_products_product_fk foreign key (tenant_id, product_id)
        references products (tenant_id, id) on delete cascade
);
create index download_products_tenant_product_idx on download_products (tenant_id, product_id);

-- ---------------------------------------------------------------------------
-- Product bundles: several products ordered together with a discount.
--
-- The discount is the bundle's own coupon, flagged `requires_all_products`
-- so it only applies when every product of the coupon is in the cart.
-- ---------------------------------------------------------------------------
alter table coupons add column requires_all_products boolean not null default false;

create table product_bundles (
    id                  uuid primary key default hf_uuid_v7(),
    tenant_id           uuid not null references tenants (id) on delete cascade,
    slug                text not null check (slug ~ '^[a-z0-9]+(?:-[a-z0-9]+)*$'),
    name                text not null check (length(name) between 1 and 200),
    description         text,
    coupon_id           uuid not null,
    is_active           boolean not null default true,
    show_on_storefront  boolean not null default true,
    sort_order          integer not null default 0,
    created_at          timestamptz not null default now(),
    updated_at          timestamptz not null default now(),
    unique (tenant_id, id),
    unique (tenant_id, slug),
    unique (tenant_id, coupon_id),
    constraint product_bundles_coupon_fk foreign key (tenant_id, coupon_id)
        references coupons (tenant_id, id) on delete restrict
);

create table product_bundle_items (
    id             uuid primary key default hf_uuid_v7(),
    tenant_id      uuid not null references tenants (id) on delete cascade,
    bundle_id      uuid not null,
    product_id     uuid not null,
    billing_cycle  text not null check (billing_cycle in (
                       'monthly', 'quarterly', 'semiannual', 'annual', 'biennial', 'triennial', 'one_time')),
    sort_order     integer not null default 0,
    unique (bundle_id, product_id),
    constraint product_bundle_items_bundle_fk foreign key (tenant_id, bundle_id)
        references product_bundles (tenant_id, id) on delete cascade,
    constraint product_bundle_items_product_fk foreign key (tenant_id, product_id)
        references products (tenant_id, id) on delete cascade
);
create index product_bundle_items_tenant_bundle_idx on product_bundle_items (tenant_id, bundle_id, sort_order);

select hf_attach_updated_at_triggers();
