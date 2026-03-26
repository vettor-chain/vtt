use thiserror::Error;

#[derive(Debug, Error)]
pub enum VmError {
    #[error("compilation error: {0}")]
    Compilation(String),
    #[error("instantiation error: {0}")]
    Instantiation(String),
    #[error("runtime error: {0}")]
    Runtime(String),
    #[error("out of gas: used {used}, limit {limit}")]
    OutOfGas { used: u64, limit: u64 },
    #[error("export not found: {0}")]
    ExportNotFound(String),
    #[error("invalid return value")]
    InvalidReturn,
    #[error("memory access error: {0}")]
    MemoryAccess(String),
}
