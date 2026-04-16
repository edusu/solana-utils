use error_stack::Report;
use thiserror::Error;

/// Top-level error categories surfaced by the CLI layer.
///
/// `RpcError` stays scoped to the `rpc` crate; the CLI wraps those reports in its own
/// variants so consumers see a single error taxonomy regardless of which layer failed.
#[derive(Debug, Error)]
pub enum CliError {
    #[error("Configuration error")]
    Config,

    #[error("RPC error")]
    Rpc,

    #[error("Invalid input")]
    Input,
}

pub type CliReport = Report<CliError>;
pub type CliResult<T> = Result<T, CliReport>;
