-- 0042: mass payment — several unpaid invoices paid with one payment.
--
-- The client picks invoices; hos-billing creates one combined invoice whose
-- lines are non-taxable `credit` lines (one per invoice, its balance at that
-- moment). That invoice is paid through the normal payment path (attempts,
-- webhooks, redirect gateways); once paid its lines become client credit
-- tied to the payment, and `mass_payment_items` then applies that credit to
-- each invoice exactly once (`applied_at`). Whatever an invoice no longer
-- needs (paid elsewhere meanwhile) stays as client credit.

create table mass_payments (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    client_id   uuid not null,
    -- The combined invoice the client pays.
    invoice_id  uuid not null,
    status      text not null default 'open' check (status in ('open', 'applied', 'cancelled')),
    applied_at  timestamptz,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    unique (tenant_id, id),
    unique (tenant_id, invoice_id),
    constraint mass_payments_client_fk foreign key (tenant_id, client_id)
        references clients (tenant_id, id) on delete cascade,
    constraint mass_payments_invoice_fk foreign key (tenant_id, invoice_id)
        references invoices (tenant_id, id) on delete cascade
);
create index mass_payments_tenant_client_idx on mass_payments (tenant_id, client_id, status);

create table mass_payment_items (
    id               uuid primary key default hf_uuid_v7(),
    tenant_id        uuid not null references tenants (id) on delete cascade,
    mass_payment_id  uuid not null,
    -- An invoice being paid.
    invoice_id       uuid not null,
    -- Its balance when the combined invoice was made.
    amount           numeric(20, 6) not null check (amount > 0),
    -- Credit actually applied (null until applied).
    applied          numeric(20, 6),
    applied_at       timestamptz,
    created_at       timestamptz not null default now(),
    unique (tenant_id, mass_payment_id, invoice_id),
    constraint mass_payment_items_mass_fk foreign key (tenant_id, mass_payment_id)
        references mass_payments (tenant_id, id) on delete cascade,
    constraint mass_payment_items_invoice_fk foreign key (tenant_id, invoice_id)
        references invoices (tenant_id, id) on delete cascade
);
create index mass_payment_items_tenant_invoice_idx on mass_payment_items (tenant_id, invoice_id);

select hf_attach_updated_at_triggers();
