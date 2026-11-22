-- HostForge schema, part 12: cross-tenant referential integrity.
--
-- Every tenant-scoped reference (`invoices.client_id -> clients.id`, ...) is
-- turned into a composite foreign key `(tenant_id, col) -> parent (tenant_id, id)`
-- so a row can never point at another tenant's row, even if application code
-- passes a foreign id. Parents get a `unique (tenant_id, id)` constraint for
-- the composite key to reference.
--
-- Rules:
--   * only single-column FKs to `id` of a parent whose `tenant_id` is NOT NULL
--     (tables with an optional tenant — jobs, scheduled_tasks, audit_log — keep
--     plain FKs when they are the parent);
--   * the ON DELETE action is preserved; `set null` becomes `set null (col)`
--     (PostgreSQL 15+) so the child's `tenant_id` is never nulled;
--   * a child with a NULL `tenant_id` (platform-level rows) is not checked
--     (MATCH SIMPLE), as before.
--
-- Written as a catalog-driven loop so that the rule is applied uniformly to
-- all tables of migrations 0002-0011.

do $$
declare
    r          record;
    del_action text;
begin
    for r in
        select c.conname,
               c.conrelid::regclass   as child,
               c.confrelid::regclass  as parent,
               pc.relname             as parent_name,
               a.attname              as col,
               c.confdeltype          as del,
               pt.attnum              as parent_tenant_attnum,
               pid.attnum             as parent_id_attnum
        from pg_constraint c
        join pg_class cc      on cc.oid = c.conrelid
        join pg_namespace n   on n.oid = cc.relnamespace
        join pg_class pc      on pc.oid = c.confrelid
        join pg_attribute a   on a.attrelid = c.conrelid and a.attnum = c.conkey[1]
        join pg_attribute ct  on ct.attrelid = c.conrelid and ct.attname = 'tenant_id' and not ct.attisdropped
        join pg_attribute pt  on pt.attrelid = c.confrelid and pt.attname = 'tenant_id' and not pt.attisdropped
        join pg_attribute pid on pid.attrelid = c.confrelid and pid.attname = 'id' and not pid.attisdropped
        where c.contype = 'f'
          and n.nspname = current_schema()
          and cardinality(c.conkey) = 1
          and c.confkey[1] = pid.attnum
          and a.attname <> 'tenant_id'
          and pc.relname <> 'tenants'
          and pt.attnotnull
        order by cc.relname, c.conname
    loop
        if not exists (
            select 1 from pg_constraint u
            where u.conrelid = r.parent
              and u.contype in ('p', 'u')
              and u.conkey = array[r.parent_tenant_attnum, r.parent_id_attnum]::int2[]
        ) then
            execute format('alter table %s add constraint %I unique (tenant_id, id)',
                           r.parent, r.parent_name || '_tenant_id_id_key');
        end if;

        del_action := case r.del
            when 'c' then 'cascade'
            when 'n' then format('set null (%I)', r.col)
            when 'd' then format('set default (%I)', r.col)
            when 'r' then 'restrict'
            else 'no action'
        end;
        execute format('alter table %s drop constraint %I', r.child, r.conname);
        execute format('alter table %s add constraint %I foreign key (tenant_id, %I) references %s (tenant_id, id) on delete %s',
                       r.child, r.conname, r.col, r.parent, del_action);
    end loop;
end
$$;
