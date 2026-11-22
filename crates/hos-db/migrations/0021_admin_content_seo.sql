-- HostForge schema, part 21: admin content / SEO tooling.
--
-- Site audits crawl the tenant's storefront page by page; the admin shows
-- live progress while the audit runs. `issues` keeps the full report
-- (`{"site": SiteReport, "geo": [...]}`) for completed audits.

alter table seo_audits
    add column pages_total integer not null default 0 check (pages_total >= 0),
    add column pages_done  integer not null default 0 check (pages_done >= 0);
