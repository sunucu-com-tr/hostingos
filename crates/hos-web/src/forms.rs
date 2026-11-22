//! Form helpers: per-field errors and small validators.
//!
//! ```ignore
//! let mut errors = FormErrors::default();
//! if !forms::valid_email(&form.email) { errors.add("email", ctx.t("auth.errors.email_invalid")); }
//! if errors.any() { return render(LoginPage { ctx, form, errors }); }   // re-render with 422
//! ```
//! In templates:
//! `{% call ui::field("email", t("auth.fields.email"), form.email, kind="email", error=errors.get("email")) %}{% endcall %}`
//! and `{% call ui::form_errors(errors) %}{% endcall %}` for form-level messages.

use std::collections::BTreeMap;

/// Validation errors keyed by field name (+ form-level messages).
#[derive(Debug, Clone, Default)]
pub struct FormErrors {
    fields: BTreeMap<String, String>,
    /// Messages not tied to one field.
    pub general: Vec<String>,
}

impl FormErrors {
    /// Records an error for `field` (the first one wins).
    pub fn add(&mut self, field: &str, message: impl Into<String>) {
        self.fields
            .entry(field.to_owned())
            .or_insert_with(|| message.into());
    }

    /// Adds a form-level message.
    pub fn add_general(&mut self, message: impl Into<String>) {
        self.general.push(message.into());
    }

    /// Error of `field`, `""` when none (template friendly).
    pub fn get(&self, field: &str) -> &str {
        self.fields.get(field).map(String::as_str).unwrap_or("")
    }

    /// Whether `field` has an error.
    pub fn has(&self, field: &str) -> bool {
        self.fields.contains_key(field)
    }

    /// Any error at all.
    pub fn any(&self) -> bool {
        !self.fields.is_empty() || !self.general.is_empty()
    }
}

/// Pragmatic e-mail check (one `@`, a dot in the domain, no spaces, ≤ 254).
pub fn valid_email(email: &str) -> bool {
    let e = email.trim();
    if e.len() > 254 || e.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let Some((local, domain)) = e.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
}

/// Normalizes an e-mail for lookups / lockout keys.
pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Trims and limits a free-text field.
pub fn clean(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

/// Name of the honeypot input rendered by `{% call ui::honeypot() %}`.
pub const HONEYPOT_FIELD: &str = "website";

/// Whether a bot filled the honeypot field (value of [`HONEYPOT_FIELD`]).
pub fn honeypot_tripped(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

/// `true` for checkbox values (`on`, `true`, `1`, `yes`).
pub fn checked(v: Option<&str>) -> bool {
    matches!(v.map(str::trim), Some("on" | "true" | "1" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails() {
        assert!(valid_email("ayse@example.com.tr"));
        assert!(valid_email(" a.b+c@x.io "));
        assert!(!valid_email("a@b"));
        assert!(!valid_email("a b@c.com"));
        assert!(!valid_email("@c.com"));
        assert!(!valid_email("a@@c.com"));
        assert!(!valid_email("a@c..com"));
    }

    #[test]
    fn errors() {
        let mut e = FormErrors::default();
        assert!(!e.any());
        e.add("email", "bad");
        e.add("email", "second");
        assert_eq!(e.get("email"), "bad");
        assert_eq!(e.get("x"), "");
        assert!(e.any());
        assert!(checked(Some("on")));
        assert!(!checked(None));
    }
}
