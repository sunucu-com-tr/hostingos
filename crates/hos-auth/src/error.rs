//! [`AuthError`]: errors of the non-OAuth modules (OAuth has its own
//! [`OAuthError`](crate::oauth::OAuthError)). Both convert into
//! [`CoreError`] so `hos-web` handlers can use `?`.

use hos_core::CoreError;
use hos_core::crypto::CryptoError;

/// Error returned by password, token, TOTP and session helpers.
///
/// Messages never contain passwords, tokens or secrets.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    /// Hashing failed (invalid parameters, RNG failure). Internal error.
    #[error("password hashing failed: {0}")]
    PasswordHash(String),
    /// A stored password hash could not be parsed (corrupt DB value).
    #[error("stored password hash is malformed")]
    MalformedHash,
    /// A presented token has the wrong shape (length, alphabet, prefix).
    #[error("token is malformed")]
    MalformedToken,
    /// A presented token does not match the stored hash.
    #[error("token is invalid")]
    InvalidToken,
    /// The token (or session) has expired.
    #[error("token has expired")]
    Expired,
    /// The one-time token was already consumed.
    #[error("token was already used")]
    AlreadyUsed,
    /// TOTP secret / URI problem.
    #[error("TOTP error: {0}")]
    Totp(String),
    /// Encryption / decoding failure.
    #[error(transparent)]
    Crypto(#[from] CryptoError),
}

impl From<AuthError> for CoreError {
    fn from(err: AuthError) -> Self {
        match err {
            AuthError::MalformedToken | AuthError::InvalidToken => {
                CoreError::Validation("invalid or unknown token".into())
            }
            AuthError::Expired => CoreError::Validation("token has expired".into()),
            AuthError::AlreadyUsed => CoreError::Validation("token was already used".into()),
            AuthError::Crypto(e) => CoreError::Crypto(e),
            other
            @ (AuthError::PasswordHash(_) | AuthError::MalformedHash | AuthError::Totp(_)) => {
                CoreError::Internal(other.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_to_core_error() {
        assert_eq!(CoreError::from(AuthError::Expired).http_status(), 400);
        assert_eq!(CoreError::from(AuthError::MalformedHash).http_status(), 500);
        assert_eq!(
            CoreError::from(AuthError::Crypto(CryptoError::Decrypt)).http_status(),
            500
        );
    }
}
