//! [`Secret<T>`]: a wrapper for credentials that never shows up in `Debug` output.
//!
//! Use it for every password, API key, token or auth code that travels through
//! request/response structs or configuration. It deliberately has **no
//! `Display`** impl; read the value with [`Secret::expose`].
//!
//! Serde is transparent in both directions (the real value is serialized) so the
//! wrapper can sit inside JSON that is later encrypted with
//! [`SecretBox`](crate::crypto::SecretBox). Never log the serialized form.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use subtle::ConstantTimeEq;

/// A value that must not leak through `Debug`/logs. See the [module docs](self).
#[derive(Clone, Default)]
pub struct Secret<T = String>(T);

/// The most common secret: a string (password, API key, token).
pub type SecretString = Secret<String>;

impl<T> Secret<T> {
    /// Wraps a value.
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Borrows the secret value. Call sites are easy to grep for — keep them few.
    pub const fn expose(&self) -> &T {
        &self.0
    }

    /// Consumes the wrapper and returns the secret value.
    pub fn into_inner(self) -> T {
        self.0
    }

    /// Transforms the inner value while keeping it wrapped.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Secret<U> {
        Secret(f(self.0))
    }
}

impl Secret<String> {
    /// Borrows the secret as `&str`.
    pub fn expose_str(&self) -> &str {
        &self.0
    }

    /// `true` when the secret is the empty string.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([REDACTED])")
    }
}

/// Equality is constant-time over the bytes (length may still leak).
impl<T: AsRef<[u8]>> PartialEq for Secret<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_ref().ct_eq(other.0.as_ref()).into()
    }
}

impl<T: AsRef<[u8]>> Eq for Secret<T> {}

impl From<String> for Secret<String> {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret<String> {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl<T: Serialize> Serialize for Secret<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = SecretString::from("hunter2");
        assert_eq!(format!("{s:?}"), "Secret([REDACTED])");
        assert!(!format!("{:?}", Some(&s)).contains("hunter2"));
    }

    #[test]
    fn serde_is_transparent() {
        let s = SecretString::from("tok");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"tok\"");
        let back: SecretString = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
        assert_eq!(back.expose_str(), "tok");
    }

    #[test]
    fn equality() {
        assert_eq!(SecretString::from("a"), SecretString::from("a"));
        assert_ne!(SecretString::from("a"), SecretString::from("b"));
        assert_ne!(SecretString::from("a"), SecretString::from("ab"));
    }
}
