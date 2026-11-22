-- HostForge schema, part 10: CMS content and SEO / GEO / AI tooling.

-- CMS pages; `slug` is the URL path below the locale root ('about', 'legal/kvkk').
create table pages (
    id                    uuid primary key default hf_uuid_v7(),
    tenant_id             uuid not null references tenants (id) on delete cascade,
    parent_id             uuid references pages (id) on delete set null,
    slug                  text not null check (slug ~ '^[a-z0-9]+(?:[-/][a-z0-9]+)*$'),
    locale                text not null check (locale in ('tr', 'en')),
    translation_group_id  uuid not null default hf_uuid_v7(),
    title                 text not null,
    -- Markdown / sanitized HTML.
    body                  text not null default '',
    template              text not null default 'default',
    status                text not null default 'draft' check (status in ('draft', 'scheduled', 'published', 'archived')),
    published_at          timestamptz,
    show_in_nav           boolean not null default false,
    show_in_footer        boolean not null default false,
    nav_order             integer not null default 0,
    author_id             uuid references users (id) on delete set null,
    created_at            timestamptz not null default now(),
    updated_at            timestamptz not null default now(),
    unique (tenant_id, locale, slug)
);
create index pages_tenant_published_idx on pages (tenant_id, locale, status, nav_order);
create index pages_translation_group_idx on pages (translation_group_id);

create table blog_posts (
    id                    uuid primary key default hf_uuid_v7(),
    tenant_id             uuid not null references tenants (id) on delete cascade,
    slug                  text not null check (slug ~ '^[a-z0-9]+(?:-[a-z0-9]+)*$'),
    locale                text not null check (locale in ('tr', 'en')),
    translation_group_id  uuid not null default hf_uuid_v7(),
    title                 text not null,
    excerpt               text,
    body                  text not null default '',
    cover_image_url       text,
    category              text,
    tags                  text[] not null default '{}',
    status                text not null default 'draft' check (status in ('draft', 'scheduled', 'published', 'archived')),
    published_at          timestamptz,
    reading_minutes       integer,
    author_id             uuid references users (id) on delete set null,
    author_name           text,
    created_at            timestamptz not null default now(),
    updated_at            timestamptz not null default now(),
    unique (tenant_id, locale, slug)
);
create index blog_posts_tenant_published_idx on blog_posts (tenant_id, locale, status, published_at desc);
create index blog_posts_translation_group_idx on blog_posts (translation_group_id);

-- SEO metadata attached to any entity (or to a static route like '/domains').
create table seo_meta (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    entity_type  text not null check (entity_type in (
                     'home', 'route', 'page', 'blog_post', 'product', 'product_group', 'kb_article',
                     'kb_category', 'announcement', 'tld')),
    -- Null for 'home' / 'route'.
    entity_id    uuid,
    -- Path for entity_type = 'route', else null.
    route        text,
    locale       text not null check (locale in ('tr', 'en')),
    title        text,
    description  text,
    canonical    text,
    keywords     text[] not null default '{}',
    -- {"title": ..., "description": ..., "image": ..., "type": ...}
    og           jsonb not null default '{}'::jsonb,
    -- Extra / overriding schema.org JSON-LD.
    jsonld       jsonb,
    noindex      boolean not null default false,
    nofollow     boolean not null default false,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    check ((entity_type = 'route') = (route is not null)),
    check (entity_type in ('home', 'route') or entity_id is not null)
);
create unique index seo_meta_target_idx
    on seo_meta (tenant_id, entity_type, entity_id, route, locale) nulls not distinct;

create table redirects (
    id           uuid primary key default hf_uuid_v7(),
    tenant_id    uuid not null references tenants (id) on delete cascade,
    from_path    text not null check (from_path like '/%'),
    to_url       text not null default '',
    status_code  smallint not null default 301 check (status_code in (301, 302, 307, 308, 410)),
    is_active    boolean not null default true,
    hits         bigint not null default 0,
    last_hit_at  timestamptz,
    note         text,
    created_at   timestamptz not null default now(),
    updated_at   timestamptz not null default now(),
    unique (tenant_id, from_path),
    check (status_code = 410 or to_url <> '')
);

create table seo_audits (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    url           text not null,
    entity_type   text,
    entity_id     uuid,
    locale        text check (locale in ('tr', 'en')),
    status        text not null default 'pending' check (status in ('pending', 'running', 'completed', 'failed')),
    score         smallint check (score between 0 and 100),
    -- [{"code": "missing_meta_description", "severity": "warning", "message": "..."}]
    issues        jsonb not null default '[]'::jsonb,
    summary       text,
    error         text,
    started_at    timestamptz,
    completed_at  timestamptz,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now()
);
create index seo_audits_tenant_created_idx on seo_audits (tenant_id, created_at desc);
create index seo_audits_tenant_entity_idx on seo_audits (tenant_id, entity_type, entity_id);

-- AI content generations (meta, posts, descriptions, FAQ, translations).
create table ai_generations (
    id              uuid primary key default hf_uuid_v7(),
    tenant_id       uuid not null references tenants (id) on delete cascade,
    user_id         uuid references users (id) on delete set null,
    integration_id  uuid references integrations (id) on delete set null,
    purpose         text not null check (purpose in (
                        'seo_meta', 'blog_post', 'product_description', 'kb_article', 'faq',
                        'translation', 'ticket_reply', 'other')),
    entity_type     text,
    entity_id       uuid,
    locale          text check (locale in ('tr', 'en')),
    model           text not null,
    prompt          text not null,
    input           jsonb not null default '{}'::jsonb,
    output          text,
    output_json     jsonb,
    status          text not null default 'pending' check (status in (
                        'pending', 'completed', 'failed', 'applied', 'discarded')),
    input_tokens    integer,
    output_tokens   integer,
    cost            numeric(20, 6),
    cost_currency   text check (cost_currency ~ '^[A-Z]{3}$'),
    error           text,
    applied_at      timestamptz,
    created_at      timestamptz not null default now(),
    updated_at      timestamptz not null default now()
);
create index ai_generations_tenant_created_idx on ai_generations (tenant_id, created_at desc);
create index ai_generations_tenant_entity_idx on ai_generations (tenant_id, entity_type, entity_id);

select hf_attach_updated_at_triggers();
