-- HostingOS schema, part 40: quotes, the affiliate programme and the
-- network status page.
--
--   * quotes / quote_items      — staff quotes (estimates) for a client or a
--                                 prospect e-mail. `token_hash` is the SHA-256
--                                 of the secret accept link (the link itself
--                                 is never stored); an accepted quote is
--                                 converted into an order + invoice
--                                 (`order_id`, `invoice_id`).
--   * affiliates                — one affiliate account per client with a
--                                 public referral `code`; optional commission
--                                 overrides.
--   * affiliate_product_rates   — per-product commission overrides.
--   * affiliate_clicks          — referral link visits, de-duplicated per
--                                 visitor (salted hash) and day.
--   * affiliate_referrals       — which affiliate referred a client (at most
--                                 one per client); `flag` holds the anti-fraud
--                                 reason that put its commissions on hold.
--   * affiliate_commissions     — commission per paid invoice line
--                                 (`invoice_item_id` unique: accrual is
--                                 idempotent); negative `adjustment` rows
--                                 take back commission that was already paid
--                                 out when its invoice is refunded later.
--   * affiliate_payouts         — payout requests (account credit or manual
--                                 payout approved by staff).
--   * status_*                  — public network status: components (optionally
--                                 bound to a server integration), incidents /
--                                 scheduled maintenance with their timeline.
--   * credits.kind              — new kind `affiliate` (payout to credit).
--   * role_permissions          — the new permissions for the built-in roles.
--
-- Additive only; composite tenant foreign keys as in migration 0012.

-- ---------------------------------------------------------------------------
-- quotes
-- ---------------------------------------------------------------------------

create table quotes (
    id                   uuid primary key default hf_uuid_v7(),
    tenant_id            uuid not null references tenants (id) on delete cascade,
    seq                  bigint not null check (seq > 0),
    client_id            uuid,
    prospect_name        text,
    prospect_email       citext check (prospect_email is null or position('@' in prospect_email) > 1),
    prospect_company     text,
    subject              text not null check (length(subject) between 1 and 200),
    status               text not null default 'draft'
                         check (status in ('draft', 'sent', 'accepted', 'declined', 'expired', 'cancelled')),
    currency             text not null check (currency ~ '^[A-Z]{3}$'),
    valid_until          date not null,
    -- Shown to the recipient (plain text / Markdown, sanitised when rendered).
    notes                text,
    admin_notes          text,
    subtotal             numeric(20, 6) not null default 0,
    discount_total       numeric(20, 6) not null default 0,
    tax_total            numeric(20, 6) not null default 0,
    total                numeric(20, 6) not null default 0,
    -- [{name, rate, is_compound, taxable_amount, tax_amount}] at the last save.
    tax_lines            jsonb not null default '[]'::jsonb,
    -- SHA-256 hex of the secret accept-link token (only the e-mail has the token).
    token_hash           text unique,
    token_expires_at     timestamptz,
    sent_at              timestamptz,
    viewed_at            timestamptz,
    accepted_at          timestamptz,
    declined_at          timestamptz,
    decline_reason       text,
    accepted_ip          inet,
    accepted_by_user_id  uuid references users (id) on delete set null,
    order_id             uuid,
    invoice_id           uuid,
    created_by           uuid references users (id) on delete set null,
    created_at           timestamptz not null default now(),
    updated_at           timestamptz not null default now(),
    unique (tenant_id, id),
    unique (tenant_id, seq),
    check (client_id is not null or prospect_email is not null),
    constraint quotes_client_fk foreign key (tenant_id, client_id)
        references clients (tenant_id, id) on delete cascade,
    constraint quotes_order_fk foreign key (tenant_id, order_id)
        references orders (tenant_id, id) on delete set null (order_id),
    constraint quotes_invoice_fk foreign key (tenant_id, invoice_id)
        references invoices (tenant_id, id) on delete set null (invoice_id)
);
create index quotes_tenant_created_idx on quotes (tenant_id, created_at desc);
create index quotes_tenant_client_idx on quotes (tenant_id, client_id, created_at desc) where client_id is not null;
create index quotes_tenant_status_idx on quotes (tenant_id, status, valid_until);

create table quote_items (
    id                uuid primary key default hf_uuid_v7(),
    tenant_id         uuid not null references tenants (id) on delete cascade,
    quote_id          uuid not null,
    product_id        uuid,
    billing_cycle     text check (billing_cycle in (
                          'monthly', 'quarterly', 'semiannual', 'annual', 'biennial', 'triennial', 'one_time')),
    description       text not null check (length(description) between 1 and 500),
    quantity          numeric(12, 4) not null default 1 check (quantity > 0 and quantity <= 100000),
    unit_price        numeric(20, 6) not null check (unit_price >= 0),
    discount_percent  numeric(9, 4) not null default 0 check (discount_percent between 0 and 100),
    -- round(quantity × unit_price × (1 − discount_percent / 100)), excluding tax.
    amount            numeric(20, 6) not null check (amount >= 0),
    taxable           boolean not null default true,
    sort_order        integer not null default 0,
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now(),
    check (product_id is null or billing_cycle is not null),
    constraint quote_items_quote_fk foreign key (tenant_id, quote_id)
        references quotes (tenant_id, id) on delete cascade,
    constraint quote_items_product_fk foreign key (tenant_id, product_id)
        references products (tenant_id, id) on delete set null (product_id)
);
create index quote_items_tenant_quote_idx on quote_items (tenant_id, quote_id, sort_order);

-- ---------------------------------------------------------------------------
-- affiliates
-- ---------------------------------------------------------------------------

create table affiliates (
    id                    uuid primary key default hf_uuid_v7(),
    tenant_id             uuid not null references tenants (id) on delete cascade,
    client_id             uuid not null,
    -- Public referral code (random, case-insensitive).
    code                  citext not null check (code ~ '^[A-Za-z0-9]{6,32}$'),
    status                text not null default 'active' check (status in ('active', 'suspended')),
    -- Overrides of the tenant defaults (null = default).
    commission_type       text check (commission_type in ('percent', 'fixed')),
    commission_value      numeric(20, 6) check (commission_value is null or commission_value >= 0),
    commission_recurring  boolean,
    currency              text not null check (currency ~ '^[A-Z]{3}$'),
    created_at            timestamptz not null default now(),
    updated_at            timestamptz not null default now(),
    unique (tenant_id, id),
    unique (tenant_id, client_id),
    unique (tenant_id, code),
    check (commission_type is null or commission_value is not null),
    check (commission_type <> 'percent' or commission_value <= 100),
    constraint affiliates_client_fk foreign key (tenant_id, client_id)
        references clients (tenant_id, id) on delete cascade
);

create table affiliate_product_rates (
    id                uuid primary key default hf_uuid_v7(),
    tenant_id         uuid not null references tenants (id) on delete cascade,
    product_id        uuid not null,
    commission_type   text not null check (commission_type in ('percent', 'fixed', 'none')),
    commission_value  numeric(20, 6) not null default 0 check (commission_value >= 0),
    recurring         boolean not null default false,
    created_at        timestamptz not null default now(),
    updated_at        timestamptz not null default now(),
    unique (tenant_id, product_id),
    check (commission_type <> 'percent' or commission_value <= 100),
    constraint affiliate_product_rates_product_fk foreign key (tenant_id, product_id)
        references products (tenant_id, id) on delete cascade
);

create table affiliate_clicks (
    id             uuid primary key default hf_uuid_v7(),
    tenant_id      uuid not null references tenants (id) on delete cascade,
    affiliate_id   uuid not null,
    -- SHA-256 of (tenant secret salt, IP, user agent, day): no raw IP stored.
    visitor_hash   text not null,
    day            date not null,
    landing_path   text,
    referrer_host  text,
    created_at     timestamptz not null default now(),
    unique (tenant_id, affiliate_id, visitor_hash, day),
    constraint affiliate_clicks_affiliate_fk foreign key (tenant_id, affiliate_id)
        references affiliates (tenant_id, id) on delete cascade
);
create index affiliate_clicks_tenant_affiliate_idx on affiliate_clicks (tenant_id, affiliate_id, day desc);

create table affiliate_referrals (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    affiliate_id  uuid not null,
    client_id     uuid not null,
    source        text not null default 'cookie' check (source in ('cookie', 'staff')),
    clicked_at    timestamptz,
    -- Anti-fraud reason that holds this referral's commissions for review.
    flag          text,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    unique (tenant_id, id),
    unique (tenant_id, client_id),
    constraint affiliate_referrals_affiliate_fk foreign key (tenant_id, affiliate_id)
        references affiliates (tenant_id, id) on delete cascade,
    constraint affiliate_referrals_client_fk foreign key (tenant_id, client_id)
        references clients (tenant_id, id) on delete cascade
);
create index affiliate_referrals_tenant_affiliate_idx on affiliate_referrals (tenant_id, affiliate_id, created_at desc);

create table affiliate_payouts (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    affiliate_id  uuid not null,
    amount        numeric(20, 6) not null check (amount > 0),
    currency      text not null check (currency ~ '^[A-Z]{3}$'),
    method        text not null check (method in ('credit', 'manual')),
    status        text not null default 'requested' check (status in ('requested', 'paid', 'rejected')),
    -- Payout details given by the affiliate (manual payouts).
    details       text,
    staff_note    text,
    processed_by  uuid references users (id) on delete set null,
    processed_at  timestamptz,
    credit_id     uuid references credits (id) on delete set null,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    unique (tenant_id, id),
    constraint affiliate_payouts_affiliate_fk foreign key (tenant_id, affiliate_id)
        references affiliates (tenant_id, id) on delete cascade
);
create index affiliate_payouts_tenant_status_idx on affiliate_payouts (tenant_id, status, created_at desc);
-- At most one open request per affiliate.
create unique index affiliate_payouts_one_open_idx on affiliate_payouts (tenant_id, affiliate_id)
    where status = 'requested';

create table affiliate_commissions (
    id               uuid primary key default hf_uuid_v7(),
    tenant_id        uuid not null references tenants (id) on delete cascade,
    affiliate_id     uuid not null,
    referral_id      uuid not null,
    client_id        uuid not null,
    invoice_id       uuid,
    invoice_item_id  uuid,
    service_id       uuid,
    kind             text not null check (kind in ('one_time', 'recurring', 'adjustment')),
    base_amount      numeric(20, 6) not null default 0,
    amount           numeric(20, 6) not null check (amount <> 0),
    currency         text not null check (currency ~ '^[A-Z]{3}$'),
    status           text not null default 'pending'
                     check (status in ('pending', 'held', 'available', 'withdrawn', 'reversed')),
    clears_at        timestamptz not null,
    reversed_at      timestamptz,
    reversal_reason  text,
    payout_id        uuid,
    -- The commission an adjustment takes back.
    adjusts_id       uuid,
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now(),
    unique (tenant_id, id),
    check (kind = 'adjustment' or amount > 0),
    check (kind <> 'adjustment' or (amount < 0 and adjusts_id is not null)),
    constraint affiliate_commissions_affiliate_fk foreign key (tenant_id, affiliate_id)
        references affiliates (tenant_id, id) on delete cascade,
    constraint affiliate_commissions_referral_fk foreign key (tenant_id, referral_id)
        references affiliate_referrals (tenant_id, id) on delete cascade,
    constraint affiliate_commissions_invoice_fk foreign key (tenant_id, invoice_id)
        references invoices (tenant_id, id) on delete set null (invoice_id),
    constraint affiliate_commissions_payout_fk foreign key (tenant_id, payout_id)
        references affiliate_payouts (tenant_id, id) on delete set null (payout_id)
);
-- Idempotent accrual: one commission per paid invoice line, one adjustment per commission.
create unique index affiliate_commissions_item_idx on affiliate_commissions (tenant_id, invoice_item_id)
    where invoice_item_id is not null and kind <> 'adjustment';
create unique index affiliate_commissions_adjusts_idx on affiliate_commissions (tenant_id, adjusts_id)
    where adjusts_id is not null;
create index affiliate_commissions_tenant_affiliate_idx on affiliate_commissions (tenant_id, affiliate_id, created_at desc);
create index affiliate_commissions_tenant_status_idx on affiliate_commissions (tenant_id, status, clears_at);
create index affiliate_commissions_tenant_invoice_idx on affiliate_commissions (tenant_id, invoice_id)
    where invoice_id is not null;

alter table credits drop constraint if exists credits_kind_check;
alter table credits add constraint credits_kind_check check (kind in (
    'manual', 'overpayment', 'refund', 'invoice_payment', 'reversal', 'promo', 'affiliate'));

-- ---------------------------------------------------------------------------
-- network status
-- ---------------------------------------------------------------------------

create table status_components (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    name            text not null check (length(name) between 1 and 120),
    group_name      text not null default '' check (length(group_name) <= 120),
    description     text,
    status          text not null default 'operational' check (status in (
                        'operational', 'degraded', 'partial_outage', 'major_outage', 'maintenance')),
    -- Server / panel / hypervisor integration whose services this component covers.
    integration_id  uuid,
    is_public       boolean not null default true,
    sort_order      integer not null default 0,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now(),
    unique (tenant_id, id),
    constraint status_components_integration_fk foreign key (tenant_id, integration_id)
        references integrations (tenant_id, id) on delete set null (integration_id)
);
create index status_components_tenant_idx on status_components (tenant_id, sort_order, name);

create table status_incidents (
    id               uuid primary key default hf_uuid_v7(),
    tenant_id        uuid not null references tenants (id) on delete cascade,
    kind             text not null default 'incident' check (kind in ('incident', 'maintenance')),
    title            text not null check (length(title) between 1 and 200),
    impact           text not null default 'minor' check (impact in ('none', 'minor', 'major', 'critical')),
    status           text not null check (status in (
                         'investigating', 'identified', 'monitoring', 'resolved',
                         'scheduled', 'in_progress', 'completed')),
    scheduled_start  timestamptz,
    scheduled_end    timestamptz,
    started_at       timestamptz not null default now(),
    resolved_at      timestamptz,
    is_public        boolean not null default true,
    notified_at      timestamptz,
    created_by       uuid references users (id) on delete set null,
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now(),
    unique (tenant_id, id),
    check (kind <> 'maintenance' or scheduled_start is not null),
    check (scheduled_end is null or scheduled_start is null or scheduled_end > scheduled_start)
);
create index status_incidents_tenant_idx on status_incidents (tenant_id, started_at desc);
create index status_incidents_open_idx on status_incidents (tenant_id, status)
    where status not in ('resolved', 'completed');

create table status_incident_components (
    tenant_id     uuid not null references tenants (id) on delete cascade,
    incident_id   uuid not null,
    component_id  uuid not null,
    primary key (tenant_id, incident_id, component_id),
    constraint status_incident_components_incident_fk foreign key (tenant_id, incident_id)
        references status_incidents (tenant_id, id) on delete cascade,
    constraint status_incident_components_component_fk foreign key (tenant_id, component_id)
        references status_components (tenant_id, id) on delete cascade
);
create index status_incident_components_component_idx on status_incident_components (tenant_id, component_id);

create table status_incident_updates (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    incident_id  uuid not null,
    status       text not null check (status in (
                     'investigating', 'identified', 'monitoring', 'resolved',
                     'scheduled', 'in_progress', 'completed')),
    body         text not null check (length(body) between 1 and 20000),
    created_by   uuid references users (id) on delete set null,
    created_at   timestamptz not null default now(),
    constraint status_incident_updates_incident_fk foreign key (tenant_id, incident_id)
        references status_incidents (tenant_id, id) on delete cascade
);
create index status_incident_updates_tenant_incident_idx on status_incident_updates (tenant_id, incident_id, created_at desc);

-- ---------------------------------------------------------------------------
-- permissions of the built-in roles (owner has `*`)
-- ---------------------------------------------------------------------------

insert into role_permissions (id, tenant_id, role_id, permission)
select hf_uuid_v7(), r.tenant_id, r.id, p.perm
  from roles r
  join (values
      ('admin', 'quotes.view'), ('admin', 'quotes.manage'),
      ('admin', 'affiliates.view'), ('admin', 'affiliates.manage'),
      ('admin', 'status.manage'), ('admin', 'tasks.manage'), ('admin', 'fraud.manage'),
      ('billing', 'quotes.view'), ('billing', 'affiliates.view'), ('billing', 'affiliates.manage'),
      ('billing', 'fraud.manage'), ('billing', 'tasks.manage'),
      ('support', 'status.manage'), ('support', 'tasks.manage'),
      ('sales', 'quotes.view'), ('sales', 'quotes.manage'), ('sales', 'affiliates.view'),
      ('sales', 'tasks.manage')
  ) as p(slug, perm) on p.slug = r.slug
 where r.is_system
on conflict (role_id, permission) do nothing;

select hf_attach_updated_at_triggers();
