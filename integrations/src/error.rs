use error_stack::Report;
use thiserror::Error;

/// Error categories surfaced by third-party integration clients (Jupiter today).
#[derive(Debug, Error)]
pub enum IntegrationError {
    /// Transport-level failure reaching the remote API (DNS, TLS, timeout, ...).
    #[error("Network error")]
    Network,

    /// The remote API returned a non-success HTTP status.
    #[error("Provider error")]
    Provider,

    /// The response body could not be parsed (JSON, base64, bincode, ...).
    #[error("Parse error")]
    Parse,

    /// The remote API returned a well-formed response that violates a domain invariant
    /// we rely on (e.g. Jupiter `/order` missing the assembled transaction).
    #[error("Jupiter error")]
    Jupiter,

    /// The Codex GraphQL API returned a well-formed response with a populated
    /// `errors` array, or with `data: null` and no errors.
    #[error("Codex error")]
    Codex,

    /// Pass-through for internal failures originating in a dependency (e.g. signing).
    #[error("Internal error")]
    Internal,
}

pub type IntegrationReport = Report<IntegrationError>;
pub type IntegrationResult<T> = Result<T, IntegrationReport>;
