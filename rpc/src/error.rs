use error_stack::Report;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("Parse error")]
    Parse,

    #[error("Provider error")]
    Provider,

    #[error("Internal error")]
    Internal,

    #[error("Network error")]
    Network,
}

pub type RpcReport = Report<RpcError>;
pub type RpcResult<T> = Result<T, RpcReport>;
