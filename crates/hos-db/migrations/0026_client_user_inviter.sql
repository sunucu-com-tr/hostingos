-- HostForge schema, part 26: who added a sub-user to a client account.
--
--   * client_users.invited_by — the client user who invited this member from
--     the client area (`/client/account/users`). Null for owners, members
--     added by staff and rows older than this column (backfilled below from
--     the `client.user.invited` audit entries where possible).
--
-- When an account whose e-mail was never verified is taken over by its real
-- owner (OAuth sign-in with a verified e-mail, completed password reset),
-- the memberships it created are removed: a pre-registered account must not
-- leave its creator a sub-user seat in the victim's client account.

alter table client_users add column invited_by uuid;

alter table client_users
    add constraint client_users_invited_by_fkey
    foreign key (tenant_id, invited_by) references users (tenant_id, id)
    on delete set null (invited_by);

create index client_users_invited_by_idx on client_users (tenant_id, invited_by)
    where invited_by is not null;

update client_users cu
   set invited_by = a.actor_id
  from audit_log a
 where a.tenant_id = cu.tenant_id
   and a.action = 'client.user.invited'
   and a.entity_type = 'user'
   and a.entity_id = cu.user_id
   and a.metadata ->> 'client_id' = cu.client_id::text
   and a.actor_id is not null
   and a.actor_id <> cu.user_id
   and not cu.is_owner
   and cu.invited_by is null
   and exists (select 1 from users u where u.tenant_id = cu.tenant_id and u.id = a.actor_id);
