//! Error helpers.
//!
//! Repository functions return [`DbResult`] (`Result<T, CoreError>`). Plain
//! `?` on a `sqlx::Error` uses hos-core's conversion (`RowNotFound` →
//! `CoreError::NotFound`, everything else → `CoreError::Database`). Where a
//! unique violation is an expected outcome (duplicate e-mail, slug, host...)
//! the repository maps it to `CoreError::Conflict` with [`conflict_on_unique`].

use hos_core::CoreError;

/// Result type of every repository function.
pub type DbResult<T> = Result<T, CoreError>;

/// SQLSTATE `unique_violation`.
pub const UNIQUE_VIOLATION: &str = "23505";
/// SQLSTATE `foreign_key_violation`.
pub const FOREIGN_KEY_VIOLATION: &str = "23503";
/// SQLSTATE `check_violation`.
pub const CHECK_VIOLATION: &str = "23514";

/// Name of the violated unique constraint/index, if `err` is a unique violation.
pub fn unique_violation(err: &sqlx::Error) -> Option<&str> {
    match err {
        sqlx::Error::Database(db) if db.code().as_deref() == Some(UNIQUE_VIOLATION) => {
            Some(db.constraint().unwrap_or(""))
        }
        _ => None,
    }
}

/// Whether a [`CoreError`] wraps a database unique violation (or is already a
/// `Conflict`).
pub fn is_unique_violation(err: &CoreError) -> bool {
    match err {
        CoreError::Conflict(_) => true,
        CoreError::Database(inner) => inner
            .downcast_ref::<sqlx::Error>()
            .is_some_and(|e| unique_violation(e).is_some()),
        _ => false,
    }
}

/// Maps unique violations to `CoreError::Conflict("<what> already exists")`,
/// foreign-key / check violations to `CoreError::Validation`, and anything
/// else through the default conversion. Use as `.map_err(conflict_on_unique("email"))`.
pub fn conflict_on_unique(what: &'static str) -> impl FnOnce(sqlx::Error) -> CoreError {
    move |err| {
        if unique_violation(&err).is_some() {
            return CoreError::Conflict(format!("{what} already exists"));
        }
        if let sqlx::Error::Database(db) = &err {
            match db.code().as_deref() {
                Some(FOREIGN_KEY_VIOLATION) => {
                    return CoreError::Validation(format!(
                        "{what}: referenced record does not exist ({})",
                        db.constraint().unwrap_or("foreign key")
                    ));
                }
                Some(CHECK_VIOLATION) => {
                    return CoreError::Validation(format!(
                        "{what}: invalid value ({})",
                        db.constraint().unwrap_or("check")
                    ));
                }
                _ => {}
            }
        }
        CoreError::from(err)
    }
}

/// `CoreError::NotFound` for `entity` / `id`.
pub fn not_found(entity: &'static str, id: impl std::fmt::Display) -> CoreError {
    CoreError::not_found(entity, id)
}

/// Escapes `%`, `_` and `\` for use inside a `LIKE`/`ILIKE` pattern and wraps
/// the term in `%...%`.
pub fn like_contains(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 2);
    out.push('%');
    for ch in term.trim().chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('%');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_escaping() {
        assert_eq!(like_contains(" a%b_c\\ "), "%a\\%b\\_c\\\\%");
        assert_eq!(like_contains(""), "%%");
    }

    #[test]
    fn conflict_mapping_passes_through_other_errors() {
        let e = conflict_on_unique("email")(sqlx::Error::RowNotFound);
        assert!(matches!(e, CoreError::NotFound { .. }));
        assert!(is_unique_violation(&CoreError::Conflict("x".into())));
        assert!(!is_unique_violation(&CoreError::Validation("x".into())));
    }
}
