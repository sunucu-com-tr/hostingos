-- 0044: e-mail piping into tickets (Mailgun inbound routes and a generic
-- authenticated raw RFC 5322 endpoint).
--
-- Replies are threaded by an unguessable per-recipient token carried in the
-- reply-to address (`support+t<token>@…`) and the subject; only its SHA-256
-- is stored. Messages that cannot be matched to a known sender are
-- quarantined for staff review instead of opening tickets.

create table mail_inbound_settings (
    tenant_id              uuid primary key references tenants (id) on delete cascade,
    enabled                boolean not null default false,
    -- Mailgun HTTP webhook signing key, sealed with the app key
    -- (hos-billing secrets: AAD `hf:mail-inbound-mailgun:` + tenant).
    mailgun_key_enc        bytea,
    -- SHA-256 (hex) of the bearer token of the raw endpoint.
    raw_token_hash         text check (raw_token_hash ~ '^[0-9a-f]{64}$'),
    -- What to do with mail from an address that belongs to no client.
    unknown_senders        text not null default 'quarantine'
                           check (unknown_senders in ('quarantine', 'guest', 'reject')),
    -- Department of new tickets when the recipient matches none.
    default_department_id  uuid,
    created_at             timestamptz not null default now(),
    updated_at             timestamptz not null default now(),
    constraint mail_inbound_settings_department_fk foreign key (tenant_id, default_department_id)
        references ticket_departments (tenant_id, id) on delete set null (default_department_id)
);

create table ticket_reply_tokens (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    ticket_id   uuid not null,
    token_hash  text not null check (token_hash ~ '^[0-9a-f]{64}$'),
    -- The address the token was sent to; a reply from another address is
    -- only accepted from the ticket's own client users.
    recipient   citext,
    expires_at  timestamptz not null,
    created_at  timestamptz not null default now(),
    unique (tenant_id, token_hash),
    constraint ticket_reply_tokens_ticket_fk foreign key (tenant_id, ticket_id)
        references tickets (tenant_id, id) on delete cascade
);
create index ticket_reply_tokens_tenant_ticket_idx on ticket_reply_tokens (tenant_id, ticket_id);

create table mail_inbound_messages (
    id               uuid primary key default hf_uuid_v7(),
    tenant_id        uuid not null references tenants (id) on delete cascade,
    source           text not null check (source in ('mailgun', 'raw')),
    -- RFC 5322 Message-ID without angle brackets (duplicate / loop guard).
    message_id       text,
    sender           citext,
    recipient        citext,
    subject          text,
    status           text not null check (status in (
                         'ticket_created', 'reply_added', 'quarantined', 'rejected', 'released', 'discarded')),
    reason           text,
    ticket_id        uuid,
    -- Kept for quarantined mail only: plain text (capped) and the names of
    -- attachments, which are not stored.
    body_text        text,
    attachment_names text[] not null default '{}',
    size_bytes       integer not null default 0 check (size_bytes >= 0),
    reviewed_by      uuid references users (id) on delete set null,
    reviewed_at      timestamptz,
    created_at       timestamptz not null default now(),
    updated_at       timestamptz not null default now(),
    unique (tenant_id, id),
    constraint mail_inbound_messages_ticket_fk foreign key (tenant_id, ticket_id)
        references tickets (tenant_id, id) on delete set null (ticket_id)
);
create unique index mail_inbound_messages_tenant_message_id_idx
    on mail_inbound_messages (tenant_id, message_id) where message_id is not null;
create index mail_inbound_messages_tenant_status_idx
    on mail_inbound_messages (tenant_id, status, created_at desc);
create index mail_inbound_messages_tenant_sender_idx
    on mail_inbound_messages (tenant_id, sender, created_at desc);

select hf_attach_updated_at_triggers();
