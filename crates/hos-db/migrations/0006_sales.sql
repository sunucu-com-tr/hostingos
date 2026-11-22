-- HostForge schema, part 6: sales — carts, orders, invoices (gap-free numbering,
-- tax breakdown), payment transactions, client credit ledger and refunds.
-- Foreign keys to services / domains are added by the later migrations that
-- create those tables.

create table carts (
    id                      uuid primary key default hf_uuid_v7(),
    tenant_id               uuid not null references tenants (id) on delete cascade,
    user_id                 uuid references users (id) on delete set null,
    client_id               uuid references clients (id) on delete set null,
    -- SHA-256 of the guest cart cookie token.
    token_hash              text unique,
    currency                text not null check (currency ~ '^[A-Z]{3}$'),
    coupon_id               uuid references coupons (id) on delete set null,
    coupon_code             citext,
    payment_integration_id  uuid references integrations (id) on delete set null,
    notes                   text,
    converted_order_id      uuid,
    expires_at              timestamptz not null default (now() + interval '30 days'),
    created_at              timestamptz not null default now(),
    updated_at              timestamptz not null default now()
);
create index carts_tenant_user_idx on carts (tenant_id, user_id) where user_id is not null;
create index carts_expires_idx on carts (expires_at);

create table cart_items (
    id                 uuid primary key default hf_uuid_v7(),
    tenant_id          uuid not null references tenants (id) on delete cascade,
    cart_id            uuid not null references carts (id) on delete cascade,
    -- Addon / bundled domain belonging to a product line.
    parent_item_id     uuid references cart_items (id) on delete cascade,
    item_type          text not null check (item_type in (
                           'product', 'addon', 'domain_register', 'domain_transfer', 'domain_renew', 'other')),
    product_id         uuid references products (id) on delete cascade,
    addon_id           uuid references product_addons (id) on delete cascade,
    domain_name        citext,
    billing_cycle      text check (billing_cycle in (
                           'monthly', 'quarterly', 'semiannual', 'annual', 'biennial', 'triennial', 'one_time')),
    years              smallint check (years between 1 and 10),
    quantity           integer not null default 1 check (quantity > 0),
    -- {"<config_option_id>": {"value_id": "<uuid>", "quantity": 2}}
    config_selections  jsonb not null default '{}'::jsonb,
    custom_fields      jsonb not null default '{}'::jsonb,
    hostname           text,
    epp_code_enc       bytea,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now(),
    check (item_type <> 'product' or product_id is not null),
    check (item_type <> 'addon' or addon_id is not null),
    check (item_type not like 'domain\_%' or domain_name is not null)
);
create index cart_items_tenant_cart_idx on cart_items (tenant_id, cart_id);

create table orders (
    id                      uuid primary key default hf_uuid_v7(),
    tenant_id               uuid not null references tenants (id) on delete cascade,
    -- Public order number (random digits, unique per tenant).
    order_number            text not null,
    client_id               uuid not null references clients (id),
    user_id                 uuid references users (id) on delete set null,
    status                  text not null default 'pending'
                            check (status in ('pending', 'accepted', 'cancelled', 'fraud')),
    currency                text not null check (currency ~ '^[A-Z]{3}$'),
    subtotal                numeric(20, 6) not null default 0,
    discount_total          numeric(20, 6) not null default 0,
    tax_total               numeric(20, 6) not null default 0,
    total                   numeric(20, 6) not null default 0,
    coupon_id               uuid references coupons (id) on delete set null,
    coupon_code             citext,
    payment_integration_id  uuid references integrations (id) on delete set null,
    invoice_id              uuid,
    ip                      inet,
    user_agent              text,
    fraud_score             numeric(9, 4),
    fraud_details           jsonb not null default '{}'::jsonb,
    notes                   text,
    admin_notes             text,
    accepted_at             timestamptz,
    cancelled_at            timestamptz,
    created_at              timestamptz not null default now(),
    updated_at              timestamptz not null default now(),
    unique (tenant_id, order_number)
);
create index orders_tenant_created_idx on orders (tenant_id, created_at desc);
create index orders_tenant_client_idx on orders (tenant_id, client_id, created_at desc);
create index orders_tenant_status_idx on orders (tenant_id, status, created_at desc);

alter table carts
    add constraint carts_converted_order_fk
    foreign key (converted_order_id) references orders (id) on delete set null;

create table order_items (
    id                 uuid primary key default hf_uuid_v7(),
    tenant_id          uuid not null references tenants (id) on delete cascade,
    order_id           uuid not null references orders (id) on delete cascade,
    parent_item_id     uuid references order_items (id) on delete cascade,
    item_type          text not null check (item_type in (
                           'product', 'addon', 'domain_register', 'domain_transfer', 'domain_renew', 'other')),
    product_id         uuid references products (id) on delete set null,
    addon_id           uuid references product_addons (id) on delete set null,
    -- Filled when the order is accepted (FKs added in later migrations).
    service_id         uuid,
    service_addon_id   uuid,
    domain_id          uuid,
    domain_name        citext,
    billing_cycle      text check (billing_cycle in (
                           'monthly', 'quarterly', 'semiannual', 'annual', 'biennial', 'triennial', 'one_time')),
    years              smallint check (years between 1 and 10),
    quantity           integer not null default 1 check (quantity > 0),
    description        text not null,
    unit_price         numeric(20, 6) not null default 0,
    setup_fee          numeric(20, 6) not null default 0,
    discount           numeric(20, 6) not null default 0,
    -- quantity * unit_price + setup_fee - discount (excluding tax).
    amount             numeric(20, 6) not null default 0,
    config_selections  jsonb not null default '{}'::jsonb,
    custom_fields      jsonb not null default '{}'::jsonb,
    hostname           text,
    epp_code_enc       bytea,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now()
);
create index order_items_tenant_order_idx on order_items (tenant_id, order_id);

-- Gap-free invoice numbering: one row per tenant, locked with FOR UPDATE while
-- the number is assigned in the same transaction as the invoice.
create table invoice_sequences (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null unique references tenants (id) on delete cascade,
    prefix        text not null default '',
    next_value    bigint not null default 1 check (next_value > 0),
    padding       smallint not null default 6 check (padding between 1 and 18),
    -- When true numbers look like '<prefix><year><padded>' and restart every year.
    reset_yearly  boolean not null default false,
    current_year  integer,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now()
);

create table invoices (
    id                      uuid primary key default hf_uuid_v7(),
    tenant_id               uuid not null references tenants (id) on delete cascade,
    client_id               uuid not null references clients (id),
    -- Assigned when the invoice is issued (drafts have none).
    number                  text,
    seq                     bigint,
    status                  text not null default 'draft' check (status in (
                                'draft', 'unpaid', 'payment_pending', 'paid', 'partially_refunded',
                                'refunded', 'cancelled', 'collections')),
    currency                text not null check (currency ~ '^[A-Z]{3}$'),
    issue_date              date not null default current_date,
    due_date                date not null,
    paid_at                 timestamptz,
    cancelled_at            timestamptz,
    refunded_at             timestamptz,
    -- Sum of item amounts (discount lines are negative items).
    subtotal                numeric(20, 6) not null default 0,
    discount_total          numeric(20, 6) not null default 0,
    tax_total               numeric(20, 6) not null default 0,
    total                   numeric(20, 6) not null default 0,
    amount_paid             numeric(20, 6) not null default 0,
    credit_applied          numeric(20, 6) not null default 0,
    balance                 numeric(20, 6) generated always as (total - amount_paid - credit_applied) stored,
    payment_integration_id  uuid references integrations (id) on delete set null,
    -- Client name/address/tax ids at issue time (legal requirement).
    billing_snapshot        jsonb not null default '{}'::jsonb,
    notes                   text,
    admin_notes             text,
    reminder_count          integer not null default 0,
    last_reminder_at        timestamptz,
    late_fee_applied_at     timestamptz,
    -- e-Arşiv / e-Fatura or accounting system reference.
    external_ref            text,
    created_at              timestamptz not null default now(),
    updated_at              timestamptz not null default now(),
    check (status in ('draft', 'cancelled') or number is not null),
    check (due_date >= issue_date)
);
create unique index invoices_tenant_number_idx on invoices (tenant_id, number) where number is not null;
create index invoices_tenant_client_idx on invoices (tenant_id, client_id, issue_date desc);
create index invoices_tenant_status_due_idx on invoices (tenant_id, status, due_date);
create index invoices_open_due_idx on invoices (due_date)
    where status in ('unpaid', 'payment_pending', 'collections');

alter table orders
    add constraint orders_invoice_fk
    foreign key (invoice_id) references invoices (id) on delete set null;

create table invoice_items (
    id                uuid primary key default hf_uuid_v7(),
    tenant_id         uuid not null references tenants (id) on delete cascade,
    invoice_id        uuid not null references invoices (id) on delete cascade,
    item_type         text not null default 'manual' check (item_type in (
                          'service', 'addon', 'domain', 'setup_fee', 'late_fee', 'credit',
                          'discount', 'upgrade', 'manual')),
    -- FKs added in later migrations.
    service_id        uuid,
    service_addon_id  uuid,
    domain_id         uuid,
    order_item_id     uuid references order_items (id) on delete set null,
    description       text not null,
    quantity          numeric(12, 4) not null default 1,
    unit_price        numeric(20, 6) not null default 0,
    -- quantity * unit_price (negative for discounts / credits), excluding tax.
    amount            numeric(20, 6) not null default 0,
    taxable           boolean not null default true,
    period_start      date,
    period_end        date,
    sort_order        integer not null default 0,
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now()
);
create index invoice_items_tenant_invoice_idx on invoice_items (tenant_id, invoice_id, sort_order);

-- Tax breakdown: the rates applied to an invoice (snapshotted from tax_rules) and
-- the computed amounts. Compound lines apply to base + preceding taxes.
create table invoice_tax_lines (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    invoice_id      uuid not null references invoices (id) on delete cascade,
    tax_rule_id     uuid references tax_rules (id) on delete set null,
    name            text not null,
    rate            numeric(9, 4) not null check (rate >= 0 and rate <= 100),
    is_compound     boolean not null default false,
    sort_order      integer not null default 0,
    taxable_amount  numeric(20, 6) not null default 0,
    tax_amount      numeric(20, 6) not null default 0,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now()
);
create index invoice_tax_lines_tenant_invoice_idx on invoice_tax_lines (tenant_id, invoice_id, sort_order);

-- Money movements through gateways (payments, refunds, chargebacks) and manual entries.
create table transactions (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    client_id       uuid not null references clients (id),
    invoice_id      uuid references invoices (id) on delete set null,
    -- Gateway integration; null for manual entries.
    integration_id  uuid references integrations (id) on delete set null,
    -- Gateway kind ('stripe', 'iyzico', 'bank_transfer', ...) or 'manual'.
    gateway         text not null,
    gateway_txn_id  text,
    kind            text not null default 'payment' check (kind in ('payment', 'refund', 'chargeback')),
    status          text not null default 'succeeded' check (status in ('pending', 'succeeded', 'failed')),
    -- Always positive; `kind` gives the direction.
    amount          numeric(20, 6) not null check (amount > 0),
    fee             numeric(20, 6) not null default 0 check (fee >= 0),
    currency        text not null check (currency ~ '^[A-Z]{3}$'),
    description     text,
    raw             jsonb not null default '{}'::jsonb,
    created_by      uuid references users (id) on delete set null,
    occurred_at     timestamptz not null default now(),
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now()
);
-- Idempotency of gateway callbacks / webhooks.
create unique index transactions_gateway_txn_idx
    on transactions (tenant_id, integration_id, gateway_txn_id) nulls not distinct
    where gateway_txn_id is not null;
create index transactions_tenant_occurred_idx on transactions (tenant_id, occurred_at desc);
create index transactions_tenant_invoice_idx on transactions (tenant_id, invoice_id);
create index transactions_tenant_client_idx on transactions (tenant_id, client_id, occurred_at desc);

-- Client credit ledger: balance = sum(amount) per client and currency.
create table credits (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    client_id       uuid not null references clients (id) on delete cascade,
    -- Positive adds credit, negative consumes it.
    amount          numeric(20, 6) not null check (amount <> 0),
    currency        text not null check (currency ~ '^[A-Z]{3}$'),
    kind            text not null check (kind in (
                        'manual', 'overpayment', 'refund', 'invoice_payment', 'reversal', 'promo')),
    description     text not null default '',
    invoice_id      uuid references invoices (id) on delete set null,
    transaction_id  uuid references transactions (id) on delete set null,
    created_by      uuid references users (id) on delete set null,
    created_at      timestamptz not null default now()
);
create index credits_tenant_client_idx on credits (tenant_id, client_id, created_at desc);

create table refunds (
    id                     uuid primary key default hf_uuid_v7(),
    tenant_id              uuid not null references tenants (id) on delete cascade,
    client_id              uuid not null references clients (id),
    transaction_id         uuid not null references transactions (id),
    invoice_id             uuid references invoices (id) on delete set null,
    -- The `transactions` row (kind = 'refund') recording the money going back.
    refund_transaction_id  uuid references transactions (id) on delete set null,
    amount                 numeric(20, 6) not null check (amount > 0),
    currency               text not null check (currency ~ '^[A-Z]{3}$'),
    reason                 text,
    status                 text not null default 'pending' check (status in ('succeeded', 'pending', 'failed')),
    -- Refund to the client's credit balance instead of the gateway.
    to_credit              boolean not null default false,
    gateway_refund_id      text,
    error                  text,
    created_by             uuid references users (id) on delete set null,
    processed_at           timestamptz,
    created_at             timestamptz not null default now(),
    updated_at             timestamptz not null default now()
);
create index refunds_tenant_created_idx on refunds (tenant_id, created_at desc);
create index refunds_tenant_transaction_idx on refunds (tenant_id, transaction_id);

select hf_attach_updated_at_triggers();
