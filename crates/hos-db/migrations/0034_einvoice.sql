-- HostingOS schema, part 34: Turkish e-invoicing (crate hos-einvoice,
-- module hos_billing::einvoice).
--
--   * integrations         — new category `einvoice` (Paraşüt, Nilvera, mock).
--   * einvoice_documents   — the legal e-document (e-Fatura / e-Arşiv) of an
--                            invoice. At most one per invoice and purpose
--                            (`sale`, or `return` for an IADE invoice), so a
--                            duplicated job can never issue a second legal
--                            document. `ettn` is fixed at creation and
--                            `request` freezes the submitted document on the
--                            first attempt, so retries resend exactly the same
--                            content. `checkpoint` is the provider's progress
--                            (multi-step APIs resume instead of starting
--                            over). `attention` is set when staff must act
--                            (refund of an e-Fatura, partial refund, failed
--                            cancellation) without changing the legal status.
--   * einvoice_sequences   — gap-free legal numbers per tenant, series and
--                            year (`AAA` + year + 9 digits) for providers
--                            where HostingOS numbers the documents. A number
--                            is allocated once per document and kept across
--                            retries.
--
-- Additive only; composite tenant FKs as in migration 0012.

alter table integrations drop constraint if exists integrations_category_check;
alter table integrations add constraint integrations_category_check
    check (category in ('virt', 'panel', 'registrar', 'gateway', 'mail', 'ai', 'oauth', 'einvoice'));

create table einvoice_documents (
    id                 uuid primary key default hf_uuid_v7(),
    tenant_id          uuid not null references tenants (id) on delete cascade,
    invoice_id         uuid not null,
    integration_id     uuid,
    purpose            text not null default 'sale' check (purpose in ('sale', 'return')),
    -- Decided on the first submission (taxpayer lookup), then frozen.
    kind               text check (kind in ('efatura', 'earsiv')),
    profile            text check (profile in ('TEMELFATURA', 'TICARIFATURA', 'EARSIVFATURA')),
    type_code          text check (type_code in ('SATIS', 'IADE', 'ISTISNA', 'TEVKIFAT')),
    ettn               uuid not null,
    number             text check (number is null or number ~ '^[A-Z0-9]{3}20[0-9]{2}[0-9]{9}$'),
    series             text check (series is null or series ~ '^[A-Z0-9]{3}$'),
    status             text not null default 'pending' check (status in (
                           'pending', 'processing', 'issued', 'failed', 'rejected', 'cancelled', 'skipped')),
    provider_id        text,
    checkpoint         jsonb not null default '{}'::jsonb,
    request            jsonb,
    is_test            boolean not null default false,
    attempts           integer not null default 0,
    last_error         text,
    -- Provider / GİB message of the current status (rejection note, …).
    message            text,
    attention          text,
    attention_at       timestamptz,
    cancellable_until  timestamptz,
    locked_until       timestamptz,
    submitted_at       timestamptz,
    issued_at          timestamptz,
    cancelled_at       timestamptz,
    created_at         timestamptz not null default now(),
    updated_at         timestamptz not null default now(),
    unique (tenant_id, id),
    unique (tenant_id, invoice_id, purpose),
    unique (tenant_id, ettn),
    constraint einvoice_documents_invoice_fk foreign key (tenant_id, invoice_id)
        references invoices (tenant_id, id) on delete cascade,
    constraint einvoice_documents_integration_fk foreign key (tenant_id, integration_id)
        references integrations (tenant_id, id) on delete set null (integration_id)
);

create unique index einvoice_documents_number_idx
    on einvoice_documents (tenant_id, number) where number is not null;
create index einvoice_documents_status_idx
    on einvoice_documents (tenant_id, status, updated_at);
create index einvoice_documents_attention_idx
    on einvoice_documents (tenant_id, attention_at) where attention is not null;

create table einvoice_sequences (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    series      text not null check (series ~ '^[A-Z0-9]{3}$'),
    year        integer not null check (year between 2000 and 2099),
    last_value  bigint not null default 0 check (last_value between 0 and 999999999),
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now(),
    unique (tenant_id, series, year)
);

select hf_attach_updated_at_triggers();
