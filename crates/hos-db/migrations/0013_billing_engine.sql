-- HostForge schema, part 13: support tables of the billing engine (hf-billing,
-- hf-jobs).
--
--   * payment_attempts  — one row per hosted payment attempt. Its `reference`
--                         is `CheckoutRequest::reference`, echoed back by the
--                         gateway as `PaymentEvent::invoice_ref`; webhooks are
--                         matched to invoices through it (or `gateway_ref`).
--   * payment_methods   — stored gateway tokens (`PaymentEvent::payment_method_ref`)
--                         for off-session renewal charges (`charge_stored`).
--   * service_upgrades  — prorated plan changes (upgrade / downgrade) waiting
--                         for their invoice to be paid, then applied.
--   * services.promo_*  — recurring coupon discounts limited to N renewals.
--
-- Composite tenant foreign keys follow migration 0012.

create table payment_attempts (
    id                   uuid primary key default hf_uuid_v7(),
    tenant_id            uuid not null references tenants (id) on delete cascade,
    invoice_id           uuid not null,
    integration_id       uuid,
    -- Alphanumeric merchant reference sent to the gateway (unique per attempt).
    reference            text not null unique check (reference ~ '^[A-Za-z0-9]{8,64}$'),
    -- Gateway session / order id (Stripe cs_..., PayPal order id, ...).
    gateway_ref          text,
    amount               numeric(20, 6) not null check (amount > 0),
    currency             text not null check (currency ~ '^[A-Z]{3}$'),
    status               text not null default 'started'
                         check (status in ('started', 'pending', 'succeeded', 'failed')),
    save_payment_method  boolean not null default false,
    created_at           timestamptz not null default now(),
    updated_at           timestamptz not null default now(),
    constraint payment_attempts_invoice_fk foreign key (tenant_id, invoice_id)
        references invoices (tenant_id, id) on delete cascade,
    constraint payment_attempts_integration_fk foreign key (tenant_id, integration_id)
        references integrations (tenant_id, id) on delete set null (integration_id)
);
create index payment_attempts_tenant_invoice_idx on payment_attempts (tenant_id, invoice_id, created_at desc);
create index payment_attempts_gateway_ref_idx on payment_attempts (tenant_id, integration_id, gateway_ref)
    where gateway_ref is not null;

create table payment_methods (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    client_id       uuid not null,
    integration_id  uuid not null,
    -- SecretBox(token) with AAD = tenant_id || id (hf-billing `payment_method_aad`).
    token_enc       bytea not null,
    -- SHA-256 hex of the token (dedupe without decrypting).
    token_hash      text not null,
    label           text,
    is_default      boolean not null default true,
    last_used_at    timestamptz,
    last_error      text,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    unique (tenant_id, integration_id, token_hash),
    constraint payment_methods_client_fk foreign key (tenant_id, client_id)
        references clients (tenant_id, id) on delete cascade,
    constraint payment_methods_integration_fk foreign key (tenant_id, integration_id)
        references integrations (tenant_id, id) on delete cascade
);
create index payment_methods_tenant_client_idx on payment_methods (tenant_id, client_id, is_default desc, created_at desc);

create table service_upgrades (
    id                    uuid primary key default hf_uuid_v7(),
    tenant_id             uuid not null references tenants (id) on delete cascade,
    service_id            uuid not null,
    invoice_id            uuid,
    from_product_id       uuid not null,
    to_product_id         uuid not null,
    from_cycle            text not null check (from_cycle in (
                              'monthly', 'quarterly', 'semiannual', 'annual', 'biennial', 'triennial', 'one_time')),
    to_cycle              text not null check (to_cycle in (
                              'monthly', 'quarterly', 'semiannual', 'annual', 'biennial', 'triennial', 'one_time')),
    currency              text not null check (currency ~ '^[A-Z]{3}$'),
    -- Prorated difference for the rest of the current period (negative = credit).
    amount                numeric(20, 6) not null,
    new_recurring_amount  numeric(20, 6) not null check (new_recurring_amount >= 0),
    config_selections     jsonb not null default '{}'::jsonb,
    status                text not null default 'pending'
                          check (status in ('pending', 'applied', 'cancelled', 'failed')),
    error                 text,
    applied_at            timestamptz,
    created_at            timestamptz not null default now(),
    updated_at            timestamptz not null default now(),
    constraint service_upgrades_service_fk foreign key (tenant_id, service_id)
        references services (tenant_id, id) on delete cascade,
    constraint service_upgrades_invoice_fk foreign key (tenant_id, invoice_id)
        references invoices (tenant_id, id) on delete set null (invoice_id),
    constraint service_upgrades_from_product_fk foreign key (tenant_id, from_product_id)
        references products (tenant_id, id),
    constraint service_upgrades_to_product_fk foreign key (tenant_id, to_product_id)
        references products (tenant_id, id)
);
create index service_upgrades_tenant_service_idx on service_upgrades (tenant_id, service_id, created_at desc);
create index service_upgrades_tenant_invoice_idx on service_upgrades (tenant_id, invoice_id) where invoice_id is not null;

-- Recurring coupon discount: `recurring_amount` is the discounted price while
-- `promo_cycles_left` > 0 (null = forever); afterwards it reverts to
-- `list_recurring_amount`.
alter table services
    add column promo_coupon_id        uuid,
    add column promo_cycles_left      integer check (promo_cycles_left is null or promo_cycles_left >= 0),
    add column list_recurring_amount  numeric(20, 6),
    add constraint services_promo_coupon_fk foreign key (tenant_id, promo_coupon_id)
        references coupons (tenant_id, id) on delete set null (promo_coupon_id);

-- Once-only markers of the daily automation (reminder / expiry notice sent
-- for a given invoice / domain and day count), independent of job retention.
create table automation_marks (
    tenant_id   uuid not null references tenants (id) on delete cascade,
    key         text not null check (length(key) between 1 and 200),
    created_at  timestamptz not null default now(),
    primary key (tenant_id, key)
);
create index automation_marks_created_idx on automation_marks (created_at);

select hf_attach_updated_at_triggers();
