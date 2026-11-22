//! A small, safe template engine for tenant-editable email templates.
//!
//! Templates come from the `email_templates` table (`subject`, `body_html`,
//! `body_text`) and are rendered against a JSON context:
//!
//! | Syntax | Meaning |
//! |---|---|
//! | `{{ name }}` | value of `name` |
//! | `{{ invoice.number }}` | nested field (array elements by index: `items.0`) |
//! | `{{#if path}} … {{else}} … {{/if}}` | conditional, may be nested; `{{else}}` optional |
//! | `{{! comment }}` | ignored |
//!
//! ## Why a purpose-built engine (and not minijinja / handlebars)
//!
//! * The requested syntax is mustache-style (`{{x}}`, `{{#if}}`). minijinja speaks
//!   Jinja (`{% if %}`), so admins would have to learn a different syntax.
//! * handlebars offers `{{{raw}}}` / `{{& raw}}` output — a one-character escape
//!   hatch for XSS in admin-editable templates — and its strict mode stops at
//!   the first missing variable.
//! * This engine has **no raw-output mode at all** (every value is escaped for
//!   its output context), reports **every** missing variable at once, has a
//!   dedicated subject-line mode that makes header injection impossible, and
//!   bounds input size and nesting depth. It is ~300 lines with no dependency
//!   beyond `serde_json`.
//!
//! ## Escaping ([`Escape`])
//!
//! * [`Escape::Html`]: values are HTML-escaped (`& < > " '`), so they are safe in
//!   element content and in quoted attributes. (A value used as an `href` is still
//!   a URL chosen by the application — only pass URLs you generated.)
//! * [`Escape::Text`]: values are inserted verbatim (plain-text bodies).
//! * [`Escape::Subject`]: control characters (CR, LF, TAB, …) and Unicode line
//!   separators in values become spaces; the final subject is collapsed to a
//!   single trimmed line, so it can never inject a header.
//!
//! ## Values and truthiness
//!
//! Strings render as-is, numbers and booleans via `to_string`, `null` as the empty
//! string. Arrays/objects cannot be printed (error). A variable that does not
//! exist is an error ([`TemplateError::MissingVariables`] lists all of them),
//! except as an `{{#if}}` condition, where missing, `null`, `false`, `""`, `0`
//! and empty arrays/objects are all false.

use serde_json::Value;

use hos_core::integrations::mailer::{EmailMessage, Mailbox};

/// Maximum template source size accepted by [`Template::parse`] (256 KiB).
pub const MAX_TEMPLATE_BYTES: usize = 256 * 1024;
/// Maximum `{{#if}}` nesting depth.
pub const MAX_NESTING: usize = 32;

/// How rendered values are escaped. See the [module docs](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    /// HTML body: values are HTML-escaped.
    Html,
    /// Plain-text body: values are inserted verbatim.
    Text,
    /// Subject line: control characters removed, output is one trimmed line.
    Subject,
}

/// Template parse / render failure. Messages never contain context values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// The template source is malformed.
    #[error("syntax error in {part} at line {line}, column {column}: {message}")]
    Syntax {
        /// Which template part (`template`, `subject`, `body_html`, `body_text`).
        part: &'static str,
        /// 1-based line.
        line: usize,
        /// 1-based column (in characters).
        column: usize,
        /// What is wrong.
        message: String,
    },
    /// Variables referenced by the template are absent from the context
    /// (deduplicated, in order of first use).
    #[error("missing template variables: {}", .0.join(", "))]
    MissingVariables(Vec<String>),
    /// Variables resolve to arrays/objects, which cannot be printed.
    #[error("template variables are not printable values: {}", .0.join(", "))]
    NotScalar(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Var(String),
    If {
        path: String,
        then: Vec<Node>,
        otherwise: Vec<Node>,
    },
}

/// A parsed template. Parse once, render many times.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    nodes: Vec<Node>,
}

struct Frame {
    path: String,
    line: usize,
    column: usize,
    then: Vec<Node>,
    otherwise: Option<Vec<Node>>,
}

impl Frame {
    fn active(&mut self) -> &mut Vec<Node> {
        match &mut self.otherwise {
            Some(o) => o,
            None => &mut self.then,
        }
    }
}

fn line_col(src: &str, offset: usize) -> (usize, usize) {
    let before = src.get(..offset).unwrap_or(src);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
}

impl Template {
    /// Parses a template. Errors are [`TemplateError::Syntax`] with `part = "template"`.
    pub fn parse(source: &str) -> Result<Self, TemplateError> {
        Self::parse_part(source, "template")
    }

    fn parse_part(src: &str, part: &'static str) -> Result<Self, TemplateError> {
        let err = |offset: usize, message: String| {
            let (line, column) = line_col(src, offset);
            TemplateError::Syntax {
                part,
                line,
                column,
                message,
            }
        };
        if src.len() > MAX_TEMPLATE_BYTES {
            return Err(err(
                0,
                format!("template is larger than {MAX_TEMPLATE_BYTES} bytes"),
            ));
        }

        let mut root: Vec<Node> = Vec::new();
        let mut stack: Vec<Frame> = Vec::new();
        let mut pos = 0;

        fn out<'a>(root: &'a mut Vec<Node>, stack: &'a mut [Frame]) -> &'a mut Vec<Node> {
            match stack.last_mut() {
                Some(frame) => frame.active(),
                None => root,
            }
        }

        while pos < src.len() {
            let rest = &src[pos..];
            let Some(open_rel) = rest.find("{{") else {
                out(&mut root, &mut stack).push(Node::Text(rest.to_owned()));
                break;
            };
            if open_rel > 0 {
                out(&mut root, &mut stack).push(Node::Text(rest[..open_rel].to_owned()));
            }
            let open = pos + open_rel;
            let inner_start = open + 2;
            let Some(close_rel) = src[inner_start..].find("}}") else {
                return Err(err(open, "unclosed tag: missing `}}`".into()));
            };
            let close = inner_start + close_rel;
            let tag = src[inner_start..close].trim();
            pos = close + 2;

            if tag.is_empty() {
                return Err(err(open, "empty tag `{{}}`".into()));
            }
            if tag.starts_with('!') {
                continue;
            }
            if tag.starts_with('{') || tag.starts_with('&') {
                return Err(err(
                    open,
                    "raw (unescaped) output is not supported; use `{{name}}`".into(),
                ));
            }
            if let Some(block) = tag.strip_prefix('#') {
                let mut words = block.split_whitespace();
                let helper = words.next().unwrap_or_default();
                if helper != "if" {
                    return Err(err(
                        open,
                        format!("unknown block `#{helper}`; only `#if` is supported"),
                    ));
                }
                let path = words.next().unwrap_or_default();
                if words.next().is_some() || !valid_path(path) {
                    return Err(err(open, "`#if` expects exactly one variable path".into()));
                }
                if stack.len() >= MAX_NESTING {
                    return Err(err(
                        open,
                        format!("`#if` nested deeper than {MAX_NESTING} levels"),
                    ));
                }
                let (line, column) = line_col(src, open);
                stack.push(Frame {
                    path: path.to_owned(),
                    line,
                    column,
                    then: Vec::new(),
                    otherwise: None,
                });
                continue;
            }
            if let Some(closing) = tag.strip_prefix('/') {
                if closing.trim() != "if" {
                    return Err(err(
                        open,
                        format!("unknown closing tag `/{}`", closing.trim()),
                    ));
                }
                let Some(frame) = stack.pop() else {
                    return Err(err(open, "`{{/if}}` without matching `{{#if}}`".into()));
                };
                let node = Node::If {
                    path: frame.path,
                    then: frame.then,
                    otherwise: frame.otherwise.unwrap_or_default(),
                };
                out(&mut root, &mut stack).push(node);
                continue;
            }
            if tag == "else" {
                match stack.last_mut() {
                    Some(frame) if frame.otherwise.is_none() => frame.otherwise = Some(Vec::new()),
                    Some(_) => return Err(err(open, "duplicate `{{else}}`".into())),
                    None => return Err(err(open, "`{{else}}` outside of `{{#if}}`".into())),
                }
                continue;
            }
            if !valid_path(tag) {
                return Err(err(
                    open,
                    "invalid variable name (use letters, digits, `_`, `-` and `.`)".into(),
                ));
            }
            out(&mut root, &mut stack).push(Node::Var(tag.to_owned()));
        }

        if let Some(frame) = stack.last() {
            return Err(TemplateError::Syntax {
                part,
                line: frame.line,
                column: frame.column,
                message: "`{{#if}}` is never closed with `{{/if}}`".into(),
            });
        }
        Ok(Self { nodes: root })
    }

    /// Every variable path referenced (printed or used as a condition),
    /// deduplicated, in order of appearance.
    pub fn variables(&self) -> Vec<String> {
        fn walk(nodes: &[Node], acc: &mut Vec<String>) {
            for node in nodes {
                match node {
                    Node::Text(_) => {}
                    Node::Var(p) => push_unique(acc, p),
                    Node::If {
                        path,
                        then,
                        otherwise,
                    } => {
                        push_unique(acc, path);
                        walk(then, acc);
                        walk(otherwise, acc);
                    }
                }
            }
        }
        let mut acc = Vec::new();
        walk(&self.nodes, &mut acc);
        acc
    }

    /// Renders against `ctx`, escaping values for `mode`.
    pub fn render(&self, ctx: &Value, mode: Escape) -> Result<String, TemplateError> {
        let mut state = RenderState::default();
        let out = self.render_into(ctx, mode, &mut state);
        state.into_result()?;
        Ok(out)
    }

    fn render_into(&self, ctx: &Value, mode: Escape, state: &mut RenderState) -> String {
        let mut out = String::new();
        render_nodes(&self.nodes, ctx, mode, state, &mut out);
        if mode == Escape::Subject {
            out = out.split_whitespace().collect::<Vec<_>>().join(" ");
        }
        out
    }
}

fn push_unique(acc: &mut Vec<String>, path: &str) {
    if !acc.iter().any(|p| p == path) {
        acc.push(path.to_owned());
    }
}

#[derive(Default)]
struct RenderState {
    missing: Vec<String>,
    not_scalar: Vec<String>,
}

impl RenderState {
    fn into_result(self) -> Result<(), TemplateError> {
        if !self.missing.is_empty() {
            return Err(TemplateError::MissingVariables(self.missing));
        }
        if !self.not_scalar.is_empty() {
            return Err(TemplateError::NotScalar(self.not_scalar));
        }
        Ok(())
    }
}

/// Resolves a dotted path (`invoice.items.0.name`) in a JSON value.
pub fn lookup<'a>(ctx: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(ctx, |cur, seg| match cur {
        Value::Object(map) => map.get(seg),
        Value::Array(items) => seg.parse::<usize>().ok().and_then(|i| items.get(i)),
        _ => None,
    })
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn render_nodes(
    nodes: &[Node],
    ctx: &Value,
    mode: Escape,
    state: &mut RenderState,
    out: &mut String,
) {
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Var(path) => match lookup(ctx, path) {
                None => push_unique(&mut state.missing, path),
                Some(Value::Null) => {}
                Some(Value::String(s)) => push_escaped(out, s, mode),
                Some(v @ (Value::Number(_) | Value::Bool(_))) => {
                    push_escaped(out, &v.to_string(), mode);
                }
                Some(Value::Array(_) | Value::Object(_)) => {
                    push_unique(&mut state.not_scalar, path);
                }
            },
            Node::If {
                path,
                then,
                otherwise,
            } => {
                let branch = if truthy(lookup(ctx, path)) {
                    then
                } else {
                    otherwise
                };
                render_nodes(branch, ctx, mode, state, out);
            }
        }
    }
}

fn push_escaped(out: &mut String, value: &str, mode: Escape) {
    match mode {
        Escape::Text => out.push_str(value),
        Escape::Html => escape_html_into(out, value),
        Escape::Subject => out.extend(value.chars().map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })),
    }
}

/// Appends `value` HTML-escaped (`& < > " '`).
pub fn escape_html_into(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// `value` HTML-escaped (`& < > " '`).
pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    escape_html_into(&mut out, value);
    out
}

/// The three parts of an `email_templates` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmailTemplateSource<'a> {
    /// Subject template (rendered with [`Escape::Subject`]).
    pub subject: &'a str,
    /// HTML body template — content only; [`crate::layout`] adds the frame.
    pub body_html: &'a str,
    /// Plain-text body template; `None`/blank → derived from the HTML.
    pub body_text: Option<&'a str>,
}

/// A rendered email (before or after [`crate::layout::apply`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedEmail {
    /// Single-line subject.
    pub subject: String,
    /// HTML body.
    pub html: String,
    /// Plain-text body.
    pub text: String,
    /// Extra headers to send (e.g. `List-Unsubscribe`, added by the layout).
    pub headers: Vec<(String, String)>,
}

impl RenderedEmail {
    /// Turns the rendered email into a sendable message (`from: None` → mailer
    /// default sender). Set `reply_to`, `tags`, attachments on the result as needed.
    pub fn into_message(self, to: Vec<Mailbox>) -> EmailMessage {
        EmailMessage {
            to,
            subject: self.subject,
            html_body: Some(self.html),
            text_body: Some(self.text),
            headers: self.headers,
            ..EmailMessage::default()
        }
    }
}

/// Renders all parts of an email template. Missing variables of every part are
/// merged into one [`TemplateError::MissingVariables`]. When `body_text` is
/// absent or blank, the text body is derived from the HTML.
pub fn render_email(
    source: &EmailTemplateSource<'_>,
    ctx: &Value,
) -> Result<RenderedEmail, TemplateError> {
    let subject = Template::parse_part(source.subject, "subject")?;
    let html = Template::parse_part(source.body_html, "body_html")?;
    let text = match source.body_text.filter(|t| !t.trim().is_empty()) {
        Some(t) => Some(Template::parse_part(t, "body_text")?),
        None => None,
    };

    let mut state = RenderState::default();
    let subject = subject.render_into(ctx, Escape::Subject, &mut state);
    let html = html.render_into(ctx, Escape::Html, &mut state);
    let text = text.map(|t| t.render_into(ctx, Escape::Text, &mut state));
    state.into_result()?;

    let text = text.unwrap_or_else(|| crate::layout::html_to_text(&html));
    Ok(RenderedEmail {
        subject,
        html,
        text,
        headers: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(src: &str, ctx: &Value, mode: Escape) -> Result<String, TemplateError> {
        Template::parse(src)?.render(ctx, mode)
    }

    #[test]
    fn variables_and_nested_paths() {
        let ctx = json!({"user": {"name": "Ayşe"}, "n": 3, "ok": true, "none": null, "items": [{"x": "a"}]});
        assert_eq!(
            render(
                "Hi {{ user.name }}! {{n}} {{ok}} [{{none}}] {{items.0.x}}",
                &ctx,
                Escape::Text
            )
            .unwrap(),
            "Hi Ayşe! 3 true [] a"
        );
    }

    #[test]
    fn html_values_are_escaped_but_template_markup_is_not() {
        let ctx = json!({"name": "<script>alert('x')</script> & \"q\""});
        let out = render("<p title=\"{{name}}\">{{name}}</p>", &ctx, Escape::Html).unwrap();
        assert_eq!(
            out,
            "<p title=\"&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;q&quot;\">\
             &lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;q&quot;</p>"
        );
        // Text mode is verbatim.
        assert_eq!(render("{{name}}", &ctx, Escape::Text).unwrap(), ctx["name"]);
    }

    #[test]
    fn subject_cannot_inject_headers() {
        let ctx = json!({"name": "Invoice\r\nBcc: evil@example.com\u{2028}x\t"});
        let out = render("Hello\n {{name}}  ", &ctx, Escape::Subject).unwrap();
        assert!(!out.contains(['\r', '\n', '\t', '\u{2028}']));
        assert_eq!(out, "Hello Invoice Bcc: evil@example.com x");
    }

    #[test]
    fn missing_variables_are_all_listed() {
        let err = render(
            "{{a}} {{b.c}} {{a}} {{#if gone}}{{inside}}{{/if}} {{#if yes}}{{d}}{{/if}}",
            &json!({"yes": 1}),
            Escape::Html,
        )
        .unwrap_err();
        assert_eq!(
            err,
            TemplateError::MissingVariables(vec!["a".into(), "b.c".into(), "d".into()])
        );
        assert!(err.to_string().contains("a, b.c, d"));
        let err = render("{{obj}}", &json!({"obj": {"k": 1}}), Escape::Html).unwrap_err();
        assert_eq!(err, TemplateError::NotScalar(vec!["obj".into()]));
    }

    #[test]
    fn if_else_nesting_and_truthiness() {
        let src = "{{#if a}}A{{#if b}}B{{else}}!B{{/if}}{{else}}!A{{/if}}";
        assert_eq!(
            render(src, &json!({"a": 1, "b": "x"}), Escape::Text).unwrap(),
            "AB"
        );
        assert_eq!(
            render(src, &json!({"a": true, "b": ""}), Escape::Text).unwrap(),
            "A!B"
        );
        assert_eq!(render(src, &json!({}), Escape::Text).unwrap(), "!A");
        for falsy in [
            json!(null),
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            assert_eq!(
                render("{{#if v}}y{{/if}}", &json!({"v": falsy}), Escape::Text).unwrap(),
                ""
            );
        }
        assert_eq!(
            render("{{#if v}}y{{/if}}", &json!({"v": [1]}), Escape::Text).unwrap(),
            "y"
        );
    }

    #[test]
    fn syntax_errors() {
        let cases = [
            ("{{name", "unclosed"),
            ("{{}}", "empty"),
            ("{{{raw}}}", "raw"),
            ("{{& raw}}", "raw"),
            ("{{#each items}}{{/each}}", "unknown block"),
            ("{{#if}}x{{/if}}", "exactly one"),
            ("{{#if a b}}x{{/if}}", "exactly one"),
            ("{{/if}}", "without matching"),
            ("{{else}}", "outside"),
            ("{{#if a}}{{else}}{{else}}{{/if}}", "duplicate"),
            ("{{#if a}}open", "never closed"),
            ("{{ bad name }}", "invalid variable"),
            ("{{a..b}}", "invalid variable"),
            ("{{>partial}}", "invalid variable"),
        ];
        for (src, needle) in cases {
            let err = Template::parse(src).unwrap_err();
            assert!(err.to_string().contains(needle), "{src}: {err}");
        }
        let err = Template::parse("line1\n  {{#bogus}}").unwrap_err();
        assert!(
            matches!(
                err,
                TemplateError::Syntax {
                    line: 2,
                    column: 3,
                    ..
                }
            ),
            "{err:?}"
        );
        let deep = "{{#if a}}".repeat(MAX_NESTING + 1);
        assert!(
            Template::parse(&deep)
                .unwrap_err()
                .to_string()
                .contains("nested")
        );
        assert!(Template::parse(&"x".repeat(MAX_TEMPLATE_BYTES + 1)).is_err());
    }

    #[test]
    fn comments_and_variables_listing() {
        let t = Template::parse("{{! note }}{{a}}{{#if b}}{{c}}{{else}}{{a}}{{/if}}").unwrap();
        assert_eq!(t.variables(), vec!["a", "b", "c"]);
        assert_eq!(
            t.render(&json!({"a": 1, "c": 2}), Escape::Text).unwrap(),
            "11"
        );
    }

    #[test]
    fn render_email_merges_errors_and_derives_text() {
        let src = EmailTemplateSource {
            subject: "Invoice {{invoice.number}}",
            body_html: "<p>Total: {{invoice.total}}</p><p>{{user.name}}</p>",
            body_text: None,
        };
        let err = render_email(&src, &json!({})).unwrap_err();
        assert_eq!(
            err,
            TemplateError::MissingVariables(vec![
                "invoice.number".into(),
                "invoice.total".into(),
                "user.name".into()
            ])
        );
        let ctx =
            json!({"invoice": {"number": "2026-1", "total": "10 €"}, "user": {"name": "A&B"}});
        let r = render_email(&src, &ctx).unwrap();
        assert_eq!(r.subject, "Invoice 2026-1");
        assert_eq!(r.html, "<p>Total: 10 €</p><p>A&amp;B</p>");
        assert_eq!(r.text, "Total: 10 €\n\nA&B");

        let bad = EmailTemplateSource {
            body_text: Some("{{#if x}}"),
            ..src
        };
        assert!(matches!(
            render_email(&bad, &ctx).unwrap_err(),
            TemplateError::Syntax {
                part: "body_text",
                ..
            }
        ));

        let msg = r.into_message(vec![Mailbox::new("a@example.com")]);
        msg.validate().unwrap();
        assert_eq!(msg.subject, "Invoice 2026-1");
    }
}
