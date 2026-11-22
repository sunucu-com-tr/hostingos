-- HostForge schema: ticket attachments stored in the database.
--
-- The client area keeps uploaded files in `ticket_attachments.content`
-- (small files, size-limited by the web layer) until an object store is
-- configured; `storage_key` is then `db:<attachment id>`. Rows written by an
-- external store keep `content` null.

alter table ticket_attachments add column content bytea;
