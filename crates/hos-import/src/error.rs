//! Import errors. Messages never contain credentials or password hashes.

use hos_core::CoreError;
use hos_core::net::OutboundError;

/// Result alias.
pub type ImportResult<T> = Result<T, ImportError>;

/// Why an import step failed.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    /// The source refused or could not be reached.
    #[error("cannot connect to the source: {0}")]
    Connect(String),
    /// The source rejected the credentials.
    #[error("the source rejected the credentials: {0}")]
    Auth(String),
    /// A read from the source failed.
    #[error("reading from the source failed: {0}")]
    Source(String),
    /// The source host is not allowed by `HOS_OUTBOUND_POLICY`.
    #[error(transparent)]
    Policy(#[from] OutboundError),
    /// Invalid wizard input / run configuration.
    #[error("invalid import configuration: {0}")]
    Config(String),
    /// The run's sealed credentials are gone (finished or wiped).
    #[error("the source credentials of this run are no longer stored; enter them again")]
    SecretsGone,
    /// The admin cancelled the run.
    #[error("cancelled")]
    Cancelled,
    /// Another worker took the run over (heartbeat lost).
    #[error("the run was taken over by another worker")]
    LeaseLost,
    /// HostingOS database / core error.
    #[error(transparent)]
    Core(#[from] CoreError),
}

impl From<sqlx::Error> for ImportError {
    fn from(e: sqlx::Error) -> Self {
        Self::Core(CoreError::from(e))
    }
}

impl ImportError {
    /// Short machine code for reports.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Connect(_) => "connect",
            Self::Auth(_) => "auth",
            Self::Source(_) => "source",
            Self::Policy(_) => "outbound_policy",
            Self::Config(_) => "config",
            Self::SecretsGone => "secrets_gone",
            Self::Cancelled => "cancelled",
            Self::LeaseLost => "lease_lost",
            Self::Core(_) => "database",
        }
    }
}
