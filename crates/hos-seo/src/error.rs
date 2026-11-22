//! Local (non-network) errors of this crate. Network calls (IndexNow, AI
//! providers) return [`hos_core::IntegrationError`] instead.

/// Error for URL normalisation, sitemap writing and similar local operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SeoError {
    /// The value is not an absolute `http(s)` URL (or is otherwise unusable).
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    /// A sitemap file is full (URL count or byte size limit reached).
    #[error("sitemap limit reached: {0}")]
    LimitReached(&'static str),
    /// Any other invalid input.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Writing to the output sink failed.
    #[error("I/O error: {0}")]
    Io(String),
}

impl SeoError {
    pub(crate) fn invalid_url(value: &str) -> Self {
        Self::InvalidUrl(value.chars().take(200).collect())
    }
}

impl From<std::io::Error> for SeoError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}
