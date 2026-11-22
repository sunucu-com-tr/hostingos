-- HostingOS schema, part 28: follow-up of the product rename to HostingOS.
--
-- The branded e-mail layout (hos-mail `layout.rs`) styles call-to-action links
-- and detail tables through CSS classes whose prefix changed from `hf-` to
-- `hos-` (`hos-button`, `hos-details`, ...). Stored templates — seeded system
-- templates and tenant edits alike — still carry the old class names, so the
-- buttons would lose their styling. Rewrite the class tokens in place; nothing
-- else in the body changes. Earlier migrations are left untouched (sqlx
-- checksums them).
--
-- `email_log` is not rewritten: its bodies are the historical copies of sent
-- messages and embed the layout stylesheet they were sent with.

update email_templates
   set body_html = regexp_replace(
           body_html,
           '\mhf-(button|details|content|container|pad)\M',
           'hos-\1',
           'g')
 where body_html ~ '\mhf-(button|details|content|container|pad)\M';
