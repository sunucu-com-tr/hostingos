-- HostForge schema, part 9: support desk (departments, tickets, messages,
-- attachments, canned responses), knowledge base and announcements.

create table ticket_departments (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    slug          text not null check (slug ~ '^[a-z0-9]+(?:-[a-z0-9]+)*$'),
    name          text not null,
    description   text,
    translations  jsonb not null default '{}'::jsonb,
    -- Inbound piping address (support@...).
    email         citext,
    clients_only  boolean not null default false,
    auto_respond  boolean not null default true,
    is_hidden     boolean not null default false,
    sort_order    integer not null default 0,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    unique (tenant_id, slug)
);

-- Staff members assigned to a department (notifications / auto-assignment).
create table ticket_department_staff (
    id             uuid primary key default hf_uuid_v7(),
    tenant_id      uuid not null references tenants (id) on delete cascade,
    department_id  uuid not null references ticket_departments (id) on delete cascade,
    user_id        uuid not null references users (id) on delete cascade,
    created_at     timestamptz not null default now(),
    unique (department_id, user_id)
);
create index ticket_department_staff_tenant_user_idx on ticket_department_staff (tenant_id, user_id);

create table tickets (
    id             uuid primary key default hf_uuid_v7(),
    tenant_id      uuid not null references tenants (id) on delete cascade,
    -- Public reference, e.g. 'TCK-482913'.
    number         text not null,
    department_id  uuid not null references ticket_departments (id),
    client_id      uuid references clients (id) on delete cascade,
    -- Opening user (client user or staff opening on behalf of a client).
    user_id        uuid references users (id) on delete set null,
    guest_name     text,
    guest_email    citext,
    subject        text not null,
    status         text not null default 'open' check (status in (
                       'open', 'answered', 'customer_reply', 'in_progress', 'on_hold', 'closed')),
    priority       text not null default 'medium' check (priority in ('low', 'medium', 'high', 'urgent')),
    service_id     uuid references services (id) on delete set null,
    domain_id      uuid references domains (id) on delete set null,
    assigned_to    uuid references users (id) on delete set null,
    last_reply_at  timestamptz not null default now(),
    last_reply_by  text not null default 'client' check (last_reply_by in ('client', 'staff', 'system', 'guest')),
    closed_at      timestamptz,
    rating         smallint check (rating between 1 and 5),
    is_flagged     boolean not null default false,
    tags           text[] not null default '{}',
    cc_emails      text[] not null default '{}',
    created_at     timestamptz not null default now(),
    updated_at     timestamptz not null default now(),
    unique (tenant_id, number),
    check (client_id is not null or guest_email is not null)
);
create index tickets_tenant_status_idx on tickets (tenant_id, status, last_reply_at desc);
create index tickets_tenant_client_idx on tickets (tenant_id, client_id, last_reply_at desc);
create index tickets_tenant_assigned_idx on tickets (tenant_id, assigned_to) where assigned_to is not null;

create table ticket_messages (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    ticket_id     uuid not null references tickets (id) on delete cascade,
    author_kind   text not null check (author_kind in ('client', 'staff', 'system', 'guest')),
    user_id       uuid references users (id) on delete set null,
    author_name   text not null default '',
    author_email  citext,
    body          text not null,
    -- Staff-only note, never shown to the client.
    is_internal   boolean not null default false,
    source        text not null default 'web' check (source in ('web', 'email', 'api')),
    -- RFC 5322 Message-ID for email threading.
    message_id    text,
    ip            inet,
    edited_at     timestamptz,
    created_at    timestamptz not null default now()
);
create index ticket_messages_tenant_ticket_idx on ticket_messages (tenant_id, ticket_id, created_at);

create table ticket_attachments (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    ticket_id     uuid not null references tickets (id) on delete cascade,
    message_id    uuid references ticket_messages (id) on delete cascade,
    filename      text not null,
    content_type  text not null default 'application/octet-stream',
    size_bytes    bigint not null check (size_bytes >= 0),
    -- Object storage key / local path.
    storage_key   text not null,
    sha256        text,
    created_at    timestamptz not null default now()
);
create index ticket_attachments_tenant_ticket_idx on ticket_attachments (tenant_id, ticket_id);

create table canned_responses (
    id          uuid primary key default hf_uuid_v7(),
    tenant_id   uuid not null references tenants (id) on delete cascade,
    title       text not null,
    body        text not null,
    locale      text not null default 'tr' check (locale in ('tr', 'en')),
    category    text,
    created_by  uuid references users (id) on delete set null,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now()
);
create index canned_responses_tenant_idx on canned_responses (tenant_id, locale, title);

create table kb_categories (
    id            uuid primary key default hf_uuid_v7(),
    tenant_id     uuid not null references tenants (id) on delete cascade,
    parent_id     uuid references kb_categories (id) on delete cascade,
    slug          text not null check (slug ~ '^[a-z0-9]+(?:-[a-z0-9]+)*$'),
    name          text not null,
    description   text,
    translations  jsonb not null default '{}'::jsonb,
    icon          text,
    is_hidden     boolean not null default false,
    sort_order    integer not null default 0,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now(),
    unique (tenant_id, slug)
);

-- One row per article per locale; translations of the same article share `translation_group_id`.
create table kb_articles (
    id                    uuid primary key default hf_uuid_v7(),
    tenant_id             uuid not null references tenants (id) on delete cascade,
    category_id           uuid references kb_categories (id) on delete set null,
    kind                  text not null default 'article' check (kind in ('article', 'faq')),
    slug                  text not null check (slug ~ '^[a-z0-9]+(?:-[a-z0-9]+)*$'),
    locale                text not null check (locale in ('tr', 'en')),
    translation_group_id  uuid not null default hf_uuid_v7(),
    title                 text not null,
    excerpt               text,
    -- Markdown.
    body                  text not null default '',
    status                text not null default 'draft' check (status in ('draft', 'scheduled', 'published', 'archived')),
    published_at          timestamptz,
    tags                  text[] not null default '{}',
    is_featured           boolean not null default false,
    sort_order            integer not null default 0,
    views                 bigint not null default 0,
    helpful_yes           integer not null default 0,
    helpful_no            integer not null default 0,
    author_id             uuid references users (id) on delete set null,
    created_at            timestamptz not null default now(),
    updated_at            timestamptz not null default now(),
    unique (tenant_id, locale, slug)
);
create index kb_articles_tenant_published_idx on kb_articles (tenant_id, locale, status, published_at desc);
create index kb_articles_tenant_category_idx on kb_articles (tenant_id, category_id, sort_order);
create index kb_articles_translation_group_idx on kb_articles (translation_group_id);

create table announcements (
    id                    uuid primary key default hf_uuid_v7(),
    tenant_id             uuid not null references tenants (id) on delete cascade,
    slug                  text not null check (slug ~ '^[a-z0-9]+(?:-[a-z0-9]+)*$'),
    locale                text not null check (locale in ('tr', 'en')),
    translation_group_id  uuid not null default hf_uuid_v7(),
    title                 text not null,
    excerpt               text,
    body                  text not null default '',
    status                text not null default 'draft' check (status in ('draft', 'scheduled', 'published', 'archived')),
    published_at          timestamptz,
    is_pinned             boolean not null default false,
    author_id             uuid references users (id) on delete set null,
    created_at            timestamptz not null default now(),
    updated_at            timestamptz not null default now(),
    unique (tenant_id, locale, slug)
);
create index announcements_tenant_published_idx on announcements (tenant_id, locale, status, published_at desc);

select hf_attach_updated_at_triggers();
