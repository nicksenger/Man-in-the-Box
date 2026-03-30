use thiserror::Error;

#[derive(Debug, Error)]
pub enum HostError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("component runtime error: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("policy error: {0}")]
    Policy(String),
    #[error("agent transport error: {0}")]
    Agent(String),
    #[error("mutex for `{0}` is poisoned")]
    PoisonedLock(&'static str),
    #[error("missing generated export: {0}")]
    MissingExport(&'static str),
}
