-- HostForge schema, part 23: billing hardening.
--
--   * service_upgrades — at most one pending plan change per service (a second
--                        change while one waits for its `change_package` job
--                        let clients farm downgrade credit). Older duplicate
--                        pending rows are cancelled before the index is built.
--   * refunds          — `integration_id` (copied from the refunded payment)
--                        and a unique gateway refund id per tenant +
--                        integration, so a refund completed synchronously is
--                        not recorded a second time by the gateway's webhook.
--   * credits          — lookup index by originating transaction (credit
--                        created by a payment is reversed when it is refunded).
--   * email_log        — `sealed_body`: the unredacted message of e-mails that
--                        carry one-time links, encrypted, kept only until the
--                        send job delivered it (`body_html` / `body_text` hold
--                        the redacted copy).

-- ---------------------------------------------------------------------------
-- service_upgrades
-- ---------------------------------------------------------------------------

update service_upgrades u
   set status = 'cancelled',
       error = 'superseded by a newer pending plan change'
 where u.status = 'pending'
   and exists (
       select 1 from service_upgrades n
        where n.tenant_id = u.tenant_id
          and n.service_id = u.service_id
          and n.status = 'pending'
          and (n.created_at, n.id) > (u.created_at, u.id)
   );

create unique index if not exists service_upgrades_one_pending_idx
    on service_upgrades (tenant_id, service_id)
    where status = 'pending';

-- ---------------------------------------------------------------------------
-- refunds
-- ---------------------------------------------------------------------------

alter table refunds add column if not exists integration_id uuid;

do $$
begin
    if not exists (select 1 from pg_constraint where conname = 'refunds_integration_fk') then
        alter table refunds
            add constraint refunds_integration_fk foreign key (tenant_id, integration_id)
            references integrations (tenant_id, id) on delete set null (integration_id);
    end if;
end
$$;

-- Backfill from the refunded payment; rows whose gateway refund id is already
-- duplicated stay without an integration (and so outside the unique index).
update refunds r
   set integration_id = t.integration_id
  from transactions t
 where t.id = r.transaction_id
   and t.tenant_id = r.tenant_id
   and t.integration_id is not null
   and r.integration_id is null
   and (
       r.gateway_refund_id is null
       or not exists (
           select 1
             from refunds r2
             join transactions t2 on t2.id = r2.transaction_id
            where r2.id <> r.id
              and r2.tenant_id = r.tenant_id
              and t2.integration_id = t.integration_id
              and r2.gateway_refund_id = r.gateway_refund_id
       )
   );

create unique index if not exists refunds_gateway_refund_idx
    on refunds (tenant_id, integration_id, gateway_refund_id)
    where gateway_refund_id is not null and integration_id is not null;

-- ---------------------------------------------------------------------------
-- credits
-- ---------------------------------------------------------------------------

create index if not exists credits_tenant_transaction_idx
    on credits (tenant_id, transaction_id)
    where transaction_id is not null;

-- ---------------------------------------------------------------------------
-- email_log
-- ---------------------------------------------------------------------------

alter table email_log add column if not exists sealed_body bytea;
