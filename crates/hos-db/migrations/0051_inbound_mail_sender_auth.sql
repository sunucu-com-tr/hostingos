-- 0051: sender authentication of piped e-mail.
--
-- The `From` header alone no longer decides which client wrote a ticket:
-- a sender counts as a known client only when sender authentication passed
-- for the From domain (see crates/hos-web/src/inbound.rs):
--
-- * generic raw endpoint: the topmost `Authentication-Results` header whose
--   authserv-id is the one the tenant names (`trusted_authserv_id`, the
--   tenant's own receiving MTA, which must remove forged copies of its
--   header); any other such header is ignored;
-- * Mailgun: `X-Mailgun-Spf: Pass` (only when the tenant says the Mailgun
--   domain marks spam with MIME headers, `mailgun_trust_spf`) for an
--   envelope sender aligned with the From domain.
--
-- Unauthenticated mail follows `unknown_senders` (quarantine by default).

alter table mail_inbound_settings
    add column trusted_authserv_id text
        check (trusted_authserv_id is null
               or trusted_authserv_id ~ '^[A-Za-z0-9]([A-Za-z0-9.-]{0,252})$'),
    add column mailgun_trust_spf boolean not null default false;

-- Messages that came by e-mail: whether the sender was authenticated
-- (null: not an e-mail, or written before this was recorded).
alter table ticket_messages add column sender_verified boolean;
alter table mail_inbound_messages add column sender_verified boolean;

-- Mailgun webhook tokens already honoured (replay protection that survives
-- restarts and is shared by every process). Rows expire with the replay
-- window and are pruned when new tokens are claimed.
create table mail_inbound_replay (
    tenant_id   uuid not null references tenants (id) on delete cascade,
    -- SHA-256 (hex) of the Mailgun `token`.
    token_hash  text not null check (token_hash ~ '^[0-9a-f]{64}$'),
    expires_at  timestamptz not null,
    created_at  timestamptz not null default now(),
    primary key (tenant_id, token_hash)
);
create index mail_inbound_replay_expires_idx on mail_inbound_replay (expires_at);
